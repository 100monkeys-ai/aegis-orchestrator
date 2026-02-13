/**
 * HTTP Server for Workflow Registration API
 * Exposes endpoints for Rust orchestrator to register workflows
 */

import express, { type Request, type Response } from 'express';
import { config } from './config.js';
import { logger } from './logger.js';
import { database } from './database.js';
import { workflowRegistry } from './workflow-registry.js';
import type { TemporalWorkflowDefinition } from './types.js';

const app = express();

// Middleware
app.use(express.json({ limit: '10mb' }));

// Health check endpoint
app.get('/health', (_req: Request, res: Response) => {
  res.json({ status: 'healthy', timestamp: new Date().toISOString() });
});

// Register workflow endpoint
// Called by Rust orchestrator after mapping Workflow domain object
app.post('/register-workflow', async (req: Request, res: Response) => {
  try {
    const definition: TemporalWorkflowDefinition = req.body;

    // Validate definition
    if (!definition.workflow_id || !definition.name || !definition.states) {
      return res.status(400).json({
        error: 'Invalid workflow definition',
        message: 'Missing required fields: workflow_id, name, or states',
      });
    }

    logger.info({ workflow_id: definition.workflow_id, name: definition.name }, 'Registering workflow');

    // Save to database (for multi-worker coordination)
    await database.saveWorkflowDefinition(definition);

    // Register with workflow registry (generates TypeScript workflow function)
    await workflowRegistry.registerWorkflow(definition);

    logger.info({ workflow_id: definition.workflow_id, name: definition.name }, 'Workflow registered successfully');

    res.status(200).json({
      status: 'registered',
      workflow_id: definition.workflow_id,
      name: definition.name,
    });
  } catch (error) {
    logger.error({ error }, 'Failed to register workflow');
    res.status(500).json({
      error: 'Registration failed',
      message: error instanceof Error ? error.message : 'Unknown error',
    });
  }
});

// Get workflow definition endpoint
// Called by Rust orchestrator to verify registration
app.get('/workflows/:id', async (req: Request, res: Response) => {
  try {
    const { id } = req.params;

    const definition = await database.getWorkflowDefinition(id);

    if (!definition) {
      return res.status(404).json({
        error: 'Workflow not found',
        workflow_id: id,
      });
    }

    res.json(definition);
  } catch (error) {
    logger.error({ error, workflow_id: req.params.id }, 'Failed to get workflow definition');
    res.status(500).json({
      error: 'Failed to retrieve workflow',
      message: error instanceof Error ? error.message : 'Unknown error',
    });
  }
});

// List all workflows endpoint
app.get('/workflows', async (_req: Request, res: Response) => {
  try {
    const definitions = await database.getAllWorkflowDefinitions();

    res.json({
      total: definitions.length,
      workflows: definitions.map((def) => ({
        workflow_id: def.workflow_id,
        name: def.name,
        version: def.version,
        state_count: Object.keys(def.states).length,
      })),
    });
  } catch (error) {
    logger.error({ error }, 'Failed to list workflows');
    res.status(500).json({
      error: 'Failed to list workflows',
      message: error instanceof Error ? error.message : 'Unknown error',
    });
  }
});

// Delete workflow endpoint
app.delete('/workflows/:id', async (req: Request, res: Response) => {
  try {
    const { id } = req.params;

    await database.deleteWorkflowDefinition(id);
    workflowRegistry.unregisterWorkflow(id);

    logger.info({ workflow_id: id }, 'Workflow deleted');

    res.status(200).json({
      status: 'deleted',
      workflow_id: id,
    });
  } catch (error) {
    logger.error({ error, workflow_id: req.params.id }, 'Failed to delete workflow');
    res.status(500).json({
      error: 'Failed to delete workflow',
      message: error instanceof Error ? error.message : 'Unknown error',
    });
  }
});

// Error handling middleware
app.use((err: Error, _req: Request, res: Response) => {
  logger.error({ err }, 'Unhandled error in HTTP server');
  res.status(500).json({
    error: 'Internal server error',
    message: err.message,
  });
});

export function startServer(): void {
  app.listen(config.http.port, config.http.host, () => {
    logger.info(
      { host: config.http.host, port: config.http.port },
      'HTTP server started - ready to accept workflow registrations'
    );
  });
}

export { app };
