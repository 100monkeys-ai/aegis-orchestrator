/**
 * Temporal Worker Initialization
 * Connects to Temporal Server and starts processing workflow/activity tasks
 */

import { NativeConnection, Worker } from '@temporalio/worker';
import { config } from './config.js';
import { logger } from './logger.js';
import { database } from './database.js';
import { workflowRegistry } from './workflow-registry.js';
import * as activities from './activities/index.js';

export async function startWorker(): Promise<void> {
  logger.info('Initializing Temporal worker...');

  try {
    // Connect to Temporal Server
    const connection = await NativeConnection.connect({
      address: config.temporal.address,
    });

    logger.info({ address: config.temporal.address }, 'Connected to Temporal Server');

    // Load all workflow definitions from database and register them
    await loadWorkflowDefinitions();

    // Get all dynamically generated workflow functions
    const workflowFunctions = workflowRegistry.getAllWorkflowFunctions();
    logger.info({ workflow_count: Object.keys(workflowFunctions).length }, 'Loaded workflow functions');

    // Create worker pointing to workflows directory
    // Using fileURLToPath for ESM compatibility
    const worker = await Worker.create({
      connection,
      namespace: config.temporal.namespace,
      taskQueue: config.temporal.taskQueue,
      workflowsPath: new URL('./workflows/index.js', import.meta.url).pathname,
      activities,
      maxConcurrentActivityTaskExecutions: config.worker.maxConcurrentActivityTaskExecutions,
      maxConcurrentWorkflowTaskExecutions: config.worker.maxConcurrentWorkflowTaskExecutions,
      bundlerOptions: {
        // Ignore non-deterministic modules that are imported by dependencies but not used at runtime
        ignoreModules: ['fs', 'path', 'os', 'crypto'],
      },
    });

    logger.info(
      {
        namespace: config.temporal.namespace,
        taskQueue: config.temporal.taskQueue,
      },
      'Temporal worker created successfully'
    );

    // Start worker
    await worker.run();

    logger.info('Temporal worker is running');
  } catch (error) {
    logger.error({ 
      error,
      message: error instanceof Error ? error.message : String(error),
      stack: error instanceof Error ? error.stack : undefined
    }, 'Failed to start Temporal worker');
    throw error;
  }
}

/**
 * Load all workflow definitions from database on worker startup
 * This ensures all workers can execute any workflow (multi-worker coordination)
 */
async function loadWorkflowDefinitions(): Promise<void> {
  try {
    logger.info('Loading workflow definitions from database...');

    const definitions = await database.getAllWorkflowDefinitions();

    logger.info({ count: definitions.length }, 'Found workflow definitions in database');

    for (const definition of definitions) {
      await workflowRegistry.registerWorkflow(definition);
      logger.info(
        { workflow_id: definition.workflow_id, name: definition.name },
        'Workflow definition loaded and registered'
      );
    }

    logger.info({ count: definitions.length }, 'All workflow definitions loaded successfully');
  } catch (error) {
    logger.error({ error }, 'Failed to load workflow definitions');
    throw error;
  }
}
