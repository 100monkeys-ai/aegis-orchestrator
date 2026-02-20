import { proxyActivities, setHandler, defineSignal, condition, workflowInfo } from '@temporalio/workflow';
import Handlebars from 'handlebars';
import type {
    TemporalWorkflowDefinition,
    WorkflowInput,
    WorkflowResult,
    Blackboard,
    WorkflowState,
    TransitionRule,
} from '../types.js';
import * as activities from '../activities/index.js';

// Proxy activities
const {
    executeAgentActivity,
    executeSystemCommandActivity,
    validateOutputActivity,
    executeParallelAgentsActivity,
    fetchWorkflowDefinition,
    publishEventActivity,
} = proxyActivities<typeof activities>({
    startToCloseTimeout: '10 minutes',
    retry: {
        maximumAttempts: 3,
    },
});

// Register Handlebars helpers (idempotent if registered multiple times in sandbox)
Handlebars.registerHelper('length', (value: any) => {
    if (Array.isArray(value)) return value.length;
    if (typeof value === 'string') return value.length;
    if (typeof value === 'object' && value !== null) return Object.keys(value).length;
    return 0;
});
Handlebars.registerHelper('upper', (str: string) => (str || '').toUpperCase());
Handlebars.registerHelper('lower', (str: string) => (str || '').toLowerCase());
Handlebars.registerHelper('trim', (str: string) => (str || '').trim());

interface GenericWorkflowInput {
    workflow_name: string;
    input: Record<string, any>;
}

/**
 * AEGIS Generic Interpreter Workflow
 * 
 * This workflow acts as an interpreter for AEGIS workflow definitions.
 * Instead of compiling definitions to creating TS code, it fetches the definition
 * at runtime and executes it step-by-step.
 */
export async function aegis_workflow(args: GenericWorkflowInput): Promise<WorkflowResult> {
    const { workflow_name, input } = args;
    const info = workflowInfo();
    const executionId = info.workflowId; // In AEGIS, Temporal workflowId is the Execution UUID
    let temporalSequenceNumber = 1;

    const emit = async (eventType: string, extra: any = {}) => {
        await publishEventActivity({
            event_type: eventType,
            execution_id: executionId,
            temporal_sequence_number: temporalSequenceNumber++,
            workflow_id: undefined,
            timestamp: new Date().toISOString(),
            ...extra
        });
    };

    await emit('WorkflowExecutionStarted');

    // 1. Fetch Definition
    const definition = await fetchWorkflowDefinition(workflow_name);

    // 2. Initialize Blackboard
    const blackboard: Blackboard = {
        ...definition.context,
        workflow: {
            name: definition.name,
            version: definition.version,
            ...input,
        },
    };

    // 3. Execution Loop
    let currentState: string | null = definition.initial_state;
    let iterationCount = 0;
    const maxIterations = 1000;

    while (currentState !== null && iterationCount < maxIterations) {
        iterationCount++;
        const state = definition.states[currentState];

        if (!state) {
            const err = `State "${currentState}" not found in definition`;
            await emit('WorkflowExecutionFailed', { error: err });
            throw new Error(err);
        }

        try {
            await emit('WorkflowStateEntered', { state_name: currentState });

            // Execute State
            const stateOutput = await executeState(state, currentState, blackboard, emit, executionId);

            await emit('WorkflowStateExited', { state_name: currentState, output: stateOutput });

            // Update Blackboard
            blackboard[currentState] = stateOutput;

            // Check Terminal
            if (!state.transitions || state.transitions.length === 0) {
                await emit('WorkflowExecutionCompleted', { final_blackboard: blackboard });
                return {
                    status: 'completed',
                    output: stateOutput,
                    iterations: iterationCount,
                    final_state: currentState,
                    blackboard,
                };
            }

            // Transition
            currentState = await evaluateTransitions(state.transitions, stateOutput, blackboard);

            if (currentState === null) {
                await emit('WorkflowExecutionCompleted', { final_blackboard: blackboard });
                return {
                    status: 'completed',
                    output: stateOutput,
                    iterations: iterationCount,
                    final_state: currentState ?? undefined,
                    blackboard,
                };
            }

        } catch (error) {
            const errMsg = error instanceof Error ? error.message : String(error);
            await emit('WorkflowExecutionFailed', { error: errMsg, final_blackboard: blackboard });
            return {
                status: 'failed',
                error: errMsg,
                iterations: iterationCount,
                final_state: currentState ?? undefined,
                blackboard,
            };
        }
    }

    const err = 'Max iterations exceeded';
    await emit('WorkflowExecutionFailed', { error: err, final_blackboard: blackboard });
    return {
        status: 'failed',
        error: err,
        iterations: iterationCount,
        final_state: currentState ?? undefined,
        blackboard,
    };
}

// ----------------------------------------------------------------------------
// Helper Functions (Same logic as workflow-generator.ts but adapted for static context)
// ----------------------------------------------------------------------------

async function executeState(
    state: WorkflowState,
    stateName: string,
    blackboard: Blackboard,
    emit: (eventType: string, extra?: any) => Promise<void>,
    executionId: string
): Promise<any> {
    switch (state.kind) {
        case 'Agent':
            if (!state.agent || !state.input) throw new Error("Invalid Agent State");

            let iteration = 1;
            let currentInput = renderTemplate(state.input, blackboard);
            const maxIterations = 3; // Temporal-level iteration bound
            let lastOutput = null;

            while (iteration <= maxIterations) {
                await emit('IterationStarted', { iteration_number: iteration });

                try {
                    const result = await executeAgentActivity({
                        agentId: state.agent,
                        input: currentInput,
                        context: blackboard,
                    });

                    lastOutput = result;
                    await emit('IterationCompleted', {
                        iteration_number: iteration,
                        output: result.output
                    });

                    if (result.status !== 'completed') {
                        throw new Error(`Agent execution failed: ${result.error}`);
                    }

                    const judges = blackboard.judges as Array<{ agent_id: string; weight?: number }>;

                    if (!judges || judges.length === 0) {
                        break;
                    }

                    const validationResult = await validateOutputActivity({
                        output: result.output,
                        judges: judges,
                    });

                    if (validationResult.binary_valid || validationResult.final_score > 0.8) {
                        break;
                    }

                    await emit('RefinementApplied', {
                        iteration_number: iteration,
                        code_diff: validationResult.reasoning,
                        agent_id: state.agent,
                    });

                    currentInput = currentInput + `\n\nValidation failed with score ${validationResult.final_score}.\nReasoning: ${validationResult.reasoning}\nPlease refine your response.`;
                    iteration++;
                } catch (error) {
                    const errMsg = error instanceof Error ? error.message : String(error);
                    await emit('IterationFailed', {
                        iteration_number: iteration,
                        error: errMsg
                    });
                    throw error;
                }
            }

            return lastOutput;

        case 'System':
            if (!state.command) throw new Error("Invalid System State");
            const env: Record<string, string> = {};
            if (state.env) {
                for (const [k, v] of Object.entries(state.env)) {
                    env[k] = renderTemplate(String(v), blackboard);
                }
            }
            return await executeSystemCommandActivity({
                command: renderTemplate(state.command, blackboard),
                env,
                workdir: state.workdir,
                timeout: state.timeout ? parseTimeout(state.timeout) : undefined,
            });

        case 'Human':
            if (!state.prompt) throw new Error("Invalid Human State");

            await emit('HumanInputRequested', { prompt: state.prompt, default_response: state.default_response });

            const humanInputSignal = defineSignal<[string]>('humanInput');
            let humanResponse: string | null = null;
            setHandler(humanInputSignal, (response) => { humanResponse = response; });

            // Wait for signal
            const timeout = state.timeout ? parseTimeout(state.timeout) : 3600;
            await condition(() => humanResponse !== null, timeout * 1000);

            if (humanResponse === null) {
                if (state.default_response) return { response: state.default_response, timeout: true };
                throw new Error("Human input timeout");
            }
            return { response: humanResponse, timeout: false };

        case 'ParallelAgents':
            if (!state.agents || !state.consensus) throw new Error("Invalid ParallelAgents State");
            const agentConfigs = state.agents.map(a => ({
                agent: a.agent,
                input: renderTemplate(a.input, blackboard),
                weight: a.weight
            }));
            return await executeParallelAgentsActivity({
                agents: agentConfigs,
                consensus: {
                    strategy: state.consensus.strategy,
                    threshold: state.consensus.threshold ?? 0.7
                }
            });

        default:
            throw new Error(`Unknown state kind: ${state.kind}`);
    }
}

async function evaluateTransitions(
    transitions: TransitionRule[],
    stateOutput: any,
    blackboard: Blackboard
): Promise<string | null> {
    for (const t of transitions) {
        if (await evaluateCondition(t, stateOutput, blackboard)) {
            return t.target;
        }
    }
    return null;
}

async function evaluateCondition(t: TransitionRule, output: any, bb: Blackboard): Promise<boolean> {
    switch (t.condition) {
        case 'always': return true;
        case 'on_success': return output?.status === 'completed' || output?.status === 'success';
        case 'on_failure': return output?.status === 'failed' || output?.status === 'error';
        case 'exit_code_zero': return output?.exit_code === 0;
        case 'exit_code_non_zero': return output?.exit_code !== 0;
        case 'exit_code': return output?.exit_code === t.exit_code;
        case 'score_above': return (output?.score || 0) > (t.threshold || 0);
        case 'score_below': return (output?.score || 0) < (t.threshold || 1);
        case 'input_equals': return output?.response === t.value;
        case 'input_equals_yes': return ['yes', 'y', '1'].includes(String(output?.response).toLowerCase());
        case 'input_equals_no': return ['no', 'n', '0'].includes(String(output?.response).toLowerCase());
        case 'custom':
            if (!t.expression) return false;
            const tmpl = `{{#if ${t.expression}}}true{{else}}false{{/if}}`;
            return renderTemplate(tmpl, { ...bb, state_output: output }).trim() === 'true';
        default: return false;
    }
}

function renderTemplate(tmpl: string, ctx: any): string {
    return Handlebars.compile(tmpl)(ctx);
}

function parseTimeout(str: string): number {
    const m = str.match(/^(\d+)([smh])$/);
    if (!m) return 60;
    const v = parseInt(m[1]);
    const u = m[2];
    if (u === 'm') return v * 60;
    if (u === 'h') return v * 3600;
    return v;
}
