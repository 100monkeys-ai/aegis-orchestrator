/**
 * Main entry point for AEGIS Temporal Worker
 * Starts both HTTP server (for workflow registration) and Temporal worker (for execution)
 */

import { logger } from './logger.js';
import { config } from './config.js';
import { database } from './database.js';
import { startServer } from './server.js';
import { startWorker } from './worker.js';

async function main() {
  logger.info('Starting AEGIS Temporal Worker...');
  logger.info({ config }, 'Configuration loaded');

  try {
    // Connect to database
    await database.connect();

    // Start HTTP server for workflow registration
    startServer();

    // Start Temporal worker for workflow execution
    await startWorker();

    logger.info('AEGIS Temporal Worker started successfully');

    // Graceful shutdown handlers
    process.on('SIGINT', async () => {
      logger.info('SIGINT received, shutting down gracefully...');
      await database.disconnect();
      process.exit(0);
    });

    process.on('SIGTERM', async () => {
      logger.info('SIGTERM received, shutting down gracefully...');
      await database.disconnect();
      process.exit(0);
    });
  } catch (error) {
    logger.error({ error }, 'Failed to start AEGIS Temporal Worker');
    process.exit(1);
  }
}

main();
