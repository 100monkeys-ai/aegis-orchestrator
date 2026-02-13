/**
 * Temporal Workflows
 * Workflow functions define the orchestration logic
 * These are dynamically generated from AEGIS workflow definitions
 */

import { workflowRegistry } from '../workflow-registry.js';

// Export all dynamically registered workflow functions
// Temporal Worker will pick up all exported functions from this file
const workflows = workflowRegistry.getAllWorkflowFunctions();

// Re-export all workflow functions
export default workflows;

// Also export them individually for Temporal's discovery
Object.entries(workflows).forEach(([name, fn]) => {
  (exports as any)[name] = fn;
});
