/**
 * Workflow Registry
 * Dynamically generates Temporal workflow functions from AEGIS workflow definitions
 * This is the core of the runtime YAML → TypeScript mapping
 */

import { logger } from './logger.js';
import { createWorkflowFromDefinition, getWorkflowName } from './workflow-generator.js';
import type { TemporalWorkflowDefinition } from './types.js';

class WorkflowRegistry {
  private workflows: Map<string, TemporalWorkflowDefinition> = new Map();
  private workflowFunctions: Map<string, Function> = new Map();

  /**
   * Register a workflow definition and generate TypeScript workflow function
   */
  async registerWorkflow(definition: TemporalWorkflowDefinition): Promise<void> {
    logger.info({ workflow_id: definition.workflow_id, name: definition.name }, 'Registering workflow in registry');

    // Store definition
    this.workflows.set(definition.workflow_id, definition);

    // Generate TypeScript workflow function from definition
    const workflowFn = createWorkflowFromDefinition(definition);
    const workflowName = getWorkflowName(definition);
    
    // Store function with Temporal-compatible name
    this.workflowFunctions.set(workflowName, workflowFn);

    logger.info({ workflow_id: definition.workflow_id, workflow_name: workflowName }, 'Workflow function generated and registered');
  }

  /**
   * Get workflow definition by ID
   */
  getWorkflow(workflowId: string): TemporalWorkflowDefinition | undefined {
    return this.workflows.get(workflowId);
  }

  /**
   * Get workflow function by workflow name (Temporal compatible)
   */
  getWorkflowFunction(workflowName: string): Function | undefined {
    return this.workflowFunctions.get(workflowName);
  }

  /**
   * Get all workflow functions for Temporal worker registration
   */
  getAllWorkflowFunctions(): Record<string, Function> {
    return Object.fromEntries(this.workflowFunctions.entries());
  }

  /**
   * Unregister workflow
   */
  unregisterWorkflow(workflowId: string): void {
    const definition = this.workflows.get(workflowId);
    if (definition) {
      const workflowName = getWorkflowName(definition);
      this.workflowFunctions.delete(workflowName);
    }
    this.workflows.delete(workflowId);
    logger.info({ workflow_id: workflowId }, 'Workflow unregistered from memory registry');
  }

  /**
   * Get all registered workflows
   */
  getAllWorkflows(): TemporalWorkflowDefinition[] {
    return Array.from(this.workflows.values());
  }

  /**
   * Clear all workflows (for testing)
   */
  clear(): void {
    this.workflows.clear();
    this.workflowFunctions.clear();
  }
}

// Singleton instance
export const workflowRegistry = new WorkflowRegistry();

// Export class for type checking
export { WorkflowRegistry };
