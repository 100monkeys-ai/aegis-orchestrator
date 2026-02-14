import { database } from '../database.js';
import { logger } from '../logger.js';
import type { TemporalWorkflowDefinition } from '../types.js';

/**
 * Fetch a workflow definition by name from the database.
 * This activity runs in the Node.js environment (outside the sandbox),
 * so it has access to the database.
 */
export async function fetchWorkflowDefinition(name: string): Promise<TemporalWorkflowDefinition> {
  logger.info({ workflow_name: name }, 'Fetching workflow definition from database');
  
  try {
    const definition = await database.getWorkflowDefinitionByName(name);
    
    if (!definition) {
      throw new Error(`Workflow definition not found: ${name}`);
    }
    
    return definition;
  } catch (error) {
    logger.error({ error, workflow_name: name }, 'Failed to fetch workflow definition');
    throw error;
  }
}
