import { proxyActivities, setHandler, defineSignal, condition } from '@temporalio/workflow';
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
            throw new Error(`State "${currentState}" not found in definition`);
        }

        try {
            // Execute State
            const stateOutput = await executeState(state, currentState, blackboard);

            // Update Blackboard
            blackboard[currentState] = stateOutput;

            // Check Terminal
            if (!state.transitions || state.transitions.length === 0) {
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
                return {
                    status: 'completed',
                    output: stateOutput,
                    iterations: iterationCount,
                    final_state: currentState ?? undefined,
                    blackboard,
                };
            }

        } catch (error) {
            return {
                status: 'failed',
                error: error instanceof Error ? error.message : String(error),
                iterations: iterationCount,
                final_state: currentState ?? undefined,
                blackboard,
            };
        }
    }

    return {
        status: 'failed',
        error: 'Max iterations exceeded',
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
    blackboard: Blackboard
): Promise<any> {
    switch (state.kind) {
        case 'Agent':
            // Validation moved to generic level provided we trust the definition schema
            if (!state.agent || !state.input) throw new Error("Invalid Agent State");
            return await executeAgentActivity({
                agentId: state.agent,
                input: renderTemplate(state.input, blackboard),
                context: blackboard,
            });

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
