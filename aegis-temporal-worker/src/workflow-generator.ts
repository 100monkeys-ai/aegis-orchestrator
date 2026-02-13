/**
 * Workflow Generator
 * Dynamically creates Temporal workflow functions from AEGIS workflow definitions
 * 
 * This is the core of the runtime YAML → TypeScript mapping.
 * It generates executable Temporal workflows from JSON definitions at runtime.
 */

import { proxyActivities, setHandler, defineSignal, defineQuery, condition } from '@temporalio/workflow';
import Handlebars from 'handlebars';
import { logger } from './logger.js';
import type {
  TemporalWorkflowDefinition,
  WorkflowInput,
  WorkflowResult,
  Blackboard,
  WorkflowState,
  TransitionRule,
} from './types.js';
import * as activities from './activities/index.js';

// Proxy activities with default timeout
const {
  executeAgentActivity,
  executeSystemCommandActivity,
  validateOutputActivity,
  executeParallelAgentsActivity,
} = proxyActivities<typeof activities>({
  startToCloseTimeout: '10 minutes',
  retry: {
    maximumAttempts: 3,
  },
});

// Register custom Handlebars helpers
Handlebars.registerHelper('length', (value: any) => {
  if (Array.isArray(value)) return value.length;
  if (typeof value === 'string') return value.length;
  if (typeof value === 'object' && value !== null) return Object.keys(value).length;
  return 0;
});

Handlebars.registerHelper('upper', (str: string) => (str || '').toUpperCase());
Handlebars.registerHelper('lower', (str: string) => (str || '').toLowerCase());
Handlebars.registerHelper('trim', (str: string) => (str || '').trim());

/**
 * Create a Temporal workflow function from an AEGIS workflow definition
 * 
 * This function generates TypeScript workflow code at runtime that:
 * 1. Implements the FSM state machine
 * 2. Executes activities based on StateKind
 * 3. Evaluates transitions
 * 4. Maintains blackboard state
 */
export function createWorkflowFromDefinition(
  definition: TemporalWorkflowDefinition
): (input: WorkflowInput) => Promise<WorkflowResult> {
  logger.info({ workflow_name: definition.name }, 'Generating Temporal workflow function');

  // Return a workflow function
  return async function generatedWorkflow(input: WorkflowInput): Promise<WorkflowResult> {
    logger.info(
      { workflow_name: definition.name, workflow_id: definition.workflow_id },
      'Starting workflow execution'
    );

    // Initialize blackboard with workflow context and input
    const blackboard: Blackboard = {
      ...definition.context,
      workflow: {
        name: definition.name,
        version: definition.version,
        ...input,
      },
    };

    // Track state transitions
    let currentState: string | null = definition.initial_state;
    let iterationCount = 0;
    const maxIterations = 1000; // Safety limit to prevent infinite loops

    // Execute FSM loop
    while (currentState !== null && iterationCount < maxIterations) {
      iterationCount++;

      const state = definition.states[currentState];
      if (!state) {
        logger.error({ current_state: currentState }, 'State not found in definition');
        return {
          status: 'failed',
          error: `State "${currentState}" not found in workflow definition`,
          iterations: iterationCount,
          final_state: currentState,
          blackboard,
        };
      }

      logger.info({ current_state: currentState, iteration: iterationCount }, 'Executing state');

      try {
        // Execute state based on kind
        const stateOutput = await executeState(state, currentState, blackboard, input);

        // Store state output in blackboard
        blackboard[currentState] = stateOutput;

        // Check if this is a terminal state (no transitions)
        if (!state.transitions || state.transitions.length === 0) {
          logger.info({ final_state: currentState }, 'Reached terminal state');
          return {
            status: 'completed',
            output: stateOutput,
            iterations: iterationCount,
            final_state: currentState,
            blackboard,
          };
        }

        // Evaluate transitions to determine next state
        currentState = await evaluateTransitions(state.transitions, stateOutput, blackboard);

        if (currentState === null) {
          logger.info('No valid transition found, workflow completed');
          return {
            status: 'completed',
            output: stateOutput,
            iterations: iterationCount,
            final_state: undefined,
            blackboard,
          };
        }
      } catch (error) {
        logger.error({ error, current_state: currentState }, 'State execution failed');
        return {
          status: 'failed',
          error: error instanceof Error ? error.message : 'Unknown error',
          iterations: iterationCount,
          final_state: currentState ?? undefined,
          blackboard,
        };
      }
    }

    // Safety limit reached
    if (iterationCount >= maxIterations) {
      logger.error({ max_iterations: maxIterations }, 'Workflow exceeded maximum iterations');
      return {
        status: 'failed',
        error: `Workflow exceeded maximum iterations (${maxIterations})`,
        iterations: iterationCount,
        final_state: currentState,
        blackboard,
      };
    }

    return {
      status: 'completed',
      iterations: iterationCount,
      final_state: currentState,
      blackboard,
    };
  };
}

/**
 * Execute a single state based on its kind
 */
async function executeState(
  state: WorkflowState,
  stateName: string,
  blackboard: Blackboard,
  input: WorkflowInput
): Promise<any> {
  switch (state.kind) {
    case 'Agent':
      return await executeAgentState(state, blackboard);

    case 'System':
      return await executeSystemState(state, blackboard);

    case 'Human':
      return await executeHumanState(state, blackboard);

    case 'ParallelAgents':
      return await executeParallelAgentsState(state, blackboard);

    default:
      throw new Error(`Unknown state kind: ${state.kind}`);
  }
}

/**
 * Execute an Agent state
 */
async function executeAgentState(state: WorkflowState, blackboard: Blackboard): Promise<any> {
  if (!state.agent || !state.input) {
    throw new Error('Agent state requires "agent" and "input" fields');
  }

  // Render input template with Handlebars
  const renderedInput = renderTemplate(state.input, blackboard);

  // Call Rust ExecutionService via gRPC activity
  const result = await executeAgentActivity({
    agentId: state.agent,
    input: renderedInput,
    context: blackboard,
  });

  return result;
}

/**
 * Execute a System command state
 */
async function executeSystemState(state: WorkflowState, blackboard: Blackboard): Promise<any> {
  if (!state.command) {
    throw new Error('System state requires "command" field');
  }

  // Render command with templates
  const renderedCommand = renderTemplate(state.command, blackboard);

  // Render environment variables
  const renderedEnv: Record<string, string> = {};
  if (state.env) {
    for (const [key, value] of Object.entries(state.env)) {
      renderedEnv[key] = renderTemplate(String(value), blackboard);
    }
  }

  // Execute system command
  const result = await executeSystemCommandActivity({
    command: renderedCommand,
    env: renderedEnv,
    workdir: state.workdir,
    timeout: state.timeout ? parseTimeout(state.timeout) : undefined,
  });

  return result;
}

/**
 * Execute a Human input state (wait for Signal)
 */
async function executeHumanState(state: WorkflowState, blackboard: Blackboard): Promise<any> {
  if (!state.prompt) {
    throw new Error('Human state requires "prompt" field');
  }

  // Define signal for human input
  const humanInputSignal = defineSignal<[string]>('humanInput');
  let humanResponse: string | null = null;

  // Set signal handler
  setHandler(humanInputSignal, (response: string) => {
    humanResponse = response;
  });

  // Render prompt
  const renderedPrompt = renderTemplate(state.prompt, blackboard);
  logger.info({ prompt: renderedPrompt }, 'Waiting for human input');

  // Wait for human input (with timeout)
  const timeout = state.timeout ? parseTimeout(state.timeout) : 3600; // Default 1 hour
  await condition(() => humanResponse !== null, timeout * 1000);

  if (humanResponse === null) {
    // Timeout - use default response if available
    if (state.default_response) {
      logger.info({ default_response: state.default_response }, 'Human input timed out, using default');
      return {
        response: state.default_response,
        timeout: true,
      };
    }
    throw new Error('Human input timeout and no default response provided');
  }

  return {
    response: humanResponse,
    timeout: false,
  };
}

/**
 * Execute ParallelAgents state (multi-judge consensus)
 */
async function executeParallelAgentsState(state: WorkflowState, blackboard: Blackboard): Promise<any> {
  if (!state.agents || !state.consensus) {
    throw new Error('ParallelAgents state requires "agents" and "consensus" fields');
  }

  // Render input for each agent
  const agentConfigs = state.agents.map((agent: { agent: string; input: string; weight: number }) => ({
    agent: agent.agent,
    input: renderTemplate(agent.input, blackboard),
    weight: agent.weight,
  }));

  // Execute all agents in parallel
  const result = await executeParallelAgentsActivity({
    agents: agentConfigs,
    consensus: {
      strategy: state.consensus.strategy,
      threshold: state.consensus.threshold ?? 0.7,
    },
  });

  return result;
}

/**
 * Evaluate transitions to determine next state
 */
async function evaluateTransitions(
  transitions: TransitionRule[],
  stateOutput: any,
  blackboard: Blackboard
): Promise<string | null> {
  for (const transition of transitions) {
    if (await evaluateTransitionCondition(transition, stateOutput, blackboard)) {
      logger.info(
        { condition: transition.condition, target: transition.target },
        'Transition condition met'
      );
      return transition.target;
    }
  }

  // No transition matched
  logger.warn('No transition condition matched, treating as terminal state');
  return null;
}

/**
 * Evaluate a single transition condition
 */
async function evaluateTransitionCondition(
  transition: TransitionRule,
  stateOutput: any,
  blackboard: Blackboard
): Promise<boolean> {
  switch (transition.condition) {
    case 'always':
      return true;

    case 'on_success':
      return stateOutput?.status === 'completed' || stateOutput?.status === 'success';

    case 'on_failure':
      return stateOutput?.status === 'failed' || stateOutput?.status === 'error';

    case 'exit_code_0':
      return stateOutput?.exit_code === 0;

    case 'exit_code_non_zero':
      return stateOutput?.exit_code !== 0;

    case 'exit_code':
      return stateOutput?.exit_code === transition.exit_code;

    case 'score_above':
      return typeof stateOutput?.score === 'number' && stateOutput.score > (transition.threshold ?? 0);

    case 'score_below':
      return typeof stateOutput?.score === 'number' && stateOutput.score < (transition.threshold ?? 1);

    case 'score_between':
      return (
        typeof stateOutput?.score === 'number' &&
        stateOutput.score >= (transition.min ?? 0) &&
        stateOutput.score <= (transition.max ?? 1)
      );

    case 'confidence_above':
      return (
        typeof stateOutput?.confidence === 'number' && stateOutput.confidence > (transition.threshold ?? 0)
      );

    case 'score_and_confidence_above':
      return (
        typeof stateOutput?.final_score === 'number' &&
        stateOutput.final_score > (transition.threshold ?? 0) &&
        typeof stateOutput?.confidence === 'number' &&
        stateOutput.confidence > (transition.threshold ?? 0)
      );

    case 'all_approved':
      return stateOutput?.individual_scores?.every((score: number) => score > 0.5) ?? false;

    case 'any_rejected':
      return stateOutput?.individual_scores?.some((score: number) => score <= 0.5) ?? false;

    case 'input_equals':
      return stateOutput?.response === transition.value;

    case 'input_equals_yes':
      return (
        stateOutput?.response?.toLowerCase() === 'yes' ||
        stateOutput?.response?.toLowerCase() === 'y' ||
        stateOutput?.response === '1'
      );

    case 'input_equals_no':
      return (
        stateOutput?.response?.toLowerCase() === 'no' ||
        stateOutput?.response?.toLowerCase() === 'n' ||
        stateOutput?.response === '0'
      );

    case 'custom':
      if (!transition.expression) {
        throw new Error('Custom transition requires "expression" field');
      }
      return evaluateCustomExpression(transition.expression, stateOutput, blackboard);

    default:
      logger.warn({ condition: transition.condition }, 'Unknown transition condition, evaluating to false');
      return false;
  }
}

/**
 * Render a Handlebars template with blackboard context
 */
function renderTemplate(template: string, blackboard: Blackboard): string {
  try {
    const compiled = Handlebars.compile(template);
    return compiled(blackboard);
  } catch (error) {
    logger.error({ error, template }, 'Failed to render template');
    throw new Error(`Template rendering failed: ${error instanceof Error ? error.message : 'Unknown error'}`);
  }
}

/**
 * Evaluate a custom boolean expression (Handlebars)
 */
function evaluateCustomExpression(expression: string, stateOutput: any, blackboard: Blackboard): boolean {
  try {
    // Wrap expression in Handlebars {{#if}} block
    const template = `{{#if ${expression}}}true{{else}}false{{/if}}`;
    const context = { ...blackboard, state_output: stateOutput };
    const result = renderTemplate(template, context);
    return result.trim() === 'true';
  } catch (error) {
    logger.error({ error, expression }, 'Failed to evaluate custom expression');
    return false;
  }
}

/**
 * Parse timeout string (e.g., "120s", "5m") to seconds
 */
function parseTimeout(timeoutStr: string): number {
  const match = timeoutStr.match(/^(\d+)([smh])$/);
  if (!match) {
    logger.warn({ timeout: timeoutStr }, 'Invalid timeout format, defaulting to 60s');
    return 60;
  }

  const value = parseInt(match[1], 10);
  const unit = match[2];

  switch (unit) {
    case 's':
      return value;
    case 'm':
      return value * 60;
    case 'h':
      return value * 3600;
    default:
      return 60;
  }
}

/**
 * Generate workflow name for Temporal registration
 */
export function getWorkflowName(definition: TemporalWorkflowDefinition): string {
  return `aegis_workflow_${definition.name.replace(/-/g, '_')}`;
}
