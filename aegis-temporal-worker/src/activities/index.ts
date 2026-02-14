/**
 * Temporal Activities
 * Activities call back to Rust services via gRPC
 */

import { logger } from '../logger.js';
import { aegisRuntimeClient } from '../grpc/client.js';
import type {
  ExecuteAgentRequest,
  ExecuteSystemCommandRequest,
  ValidateRequest,
  Blackboard,
} from '../types.js';
import { fetchWorkflowDefinition } from './workflow-activities.js';

/**
 * Execute an agent via Rust ExecutionService
 */
export async function executeAgentActivity(params: {
  agentId: string;
  input: string;
  context: Blackboard;
}): Promise<any> {
  logger.info({ agent_id: params.agentId }, 'Executing agent activity');

  // Build context map with input and blackboard data
  const contextMap: Record<string, string> = {
    input: params.input,
    ...Object.fromEntries(
      Object.entries(params.context).map(([k, v]) => [k, String(v)])
    ),
  };

  const request: ExecuteAgentRequest = {
    agent_id: params.agentId,
    context: contextMap,
    timeout_seconds: 300,
  };

  try {
    // Call Rust ExecutionService via gRPC (streaming)
    const events = await aegisRuntimeClient.executeAgent(request);

    // Extract final result from events
    const completedEvent = events.find(e => e.event_type === 'ExecutionCompleted');
    const failedEvent = events.find(e => e.event_type === 'ExecutionFailed');

    if (completedEvent) {
      return {
        status: 'completed',
        output: completedEvent.final_output || '',
        iterations: completedEvent.total_iterations || 0,
      };
    }

    if (failedEvent) {
      return {
        status: 'failed',
        error: failedEvent.reason || 'Unknown error',
        iterations: failedEvent.total_iterations || 0,
      };
    }

    throw new Error('No completion or failure event received');
  } catch (error) {
    logger.error({ error, agent_id: params.agentId }, 'Agent execution activity failed');
    throw error;
  }
}

/**
 * Execute a system command
 */
export async function executeSystemCommandActivity(params: {
  command: string;
  env?: Record<string, string>;
  workdir?: string;
  timeout?: number;
}): Promise<any> {
  logger.info({ command: params.command }, 'Executing system command activity');

  const request: ExecuteSystemCommandRequest = {
    command: params.command,
    env: params.env || {},
    workdir: params.workdir,
    timeout_seconds: params.timeout,
  };

  try {
    const response = await aegisRuntimeClient.executeSystemCommand(request);

    return {
      status: response.exit_code === 0 ? 'success' : 'failed',
      exit_code: response.exit_code,
      stdout: response.stdout,
      stderr: response.stderr,
    };
  } catch (error) {
    logger.error({ error, command: params.command }, 'System command activity failed');
    throw error;
  }
}

/**
 * Validate output with judge agents
 */
export async function validateOutputActivity(params: {
  output: string;
  judges: Array<{ agent_id: string; weight?: number }>;
  consensus_strategy?: string;
  consensus_threshold?: number;
}): Promise<any> {
  logger.info({ judge_count: params.judges.length }, 'Validating output with judges');

  const request: ValidateRequest = {
    agent_output: params.output,
    judge_agent_ids: params.judges.map(j => j.agent_id),
    context: {},
  };

  try {
    const response = await aegisRuntimeClient.validateWithJudges(request);

    return {
      final_score: response.final_score,
      confidence: response.confidence,
      individual_scores: response.individual_scores,
      reasoning: response.reasoning,
    };
  } catch (error) {
    logger.error({ error }, 'Validation activity failed');
    throw error;
  }
}

/**
 * Execute multiple agents in parallel
 */
export async function executeParallelAgentsActivity(params: {
  agents: Array<{ agent: string; input: string; weight?: number }>;
  consensus: {
    strategy: string;
    threshold: number;
  };
}): Promise<any> {
  logger.info({ agent_count: params.agents.length }, 'Executing parallel agents');

  try {
    // Execute all agents in parallel
    const results = await Promise.all(
      params.agents.map(async (agentConfig) => {
        const events = await aegisRuntimeClient.executeAgent({
          agent_id: agentConfig.agent,
          context: { input: agentConfig.input },
          timeout_seconds: 300,
        });

        // Extract output
        const completedEvent = events.find(e => e.event_type === 'ExecutionCompleted');
        if (completedEvent) {
          return {
            output: completedEvent.final_output || '',
            weight: agentConfig.weight || 1.0,
          };
        }

        throw new Error(`Agent ${agentConfig.agent} did not complete successfully`);
      })
    );

    // All agents completed - now validate with consensus
    const outputsForValidation = results.map(r => r.output).join('\n\n---\n\n');

    const validationResult = await aegisRuntimeClient.validateWithJudges({
      agent_output: outputsForValidation,
      judge_agent_ids: params.agents.map(a => a.agent),
      context: {},
    });

    return {
      individual_outputs: results.map(r => r.output),
      individual_scores: validationResult.individual_scores,
      final_score: validationResult.final_score,
      confidence: validationResult.confidence,
      reasoning: validationResult.reasoning,
    };
  } catch (error) {
    logger.error({ error }, 'Parallel agents activity failed');
    throw error;
  }
}

// Ensure all activities are exported for Temporal Worker
export const activities = {
  executeAgentActivity,
  executeSystemCommandActivity,
  validateOutputActivity,
  executeParallelAgentsActivity,
  fetchWorkflowDefinition,
};

export { fetchWorkflowDefinition };


