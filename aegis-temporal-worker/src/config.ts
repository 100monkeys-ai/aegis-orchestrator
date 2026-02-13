/**
 * AEGIS Temporal Worker Configuration
 * Loads environment variables and provides typed config
 */

import dotenv from 'dotenv';
import { z } from 'zod';

dotenv.config();

const configSchema = z.object({
  temporal: z.object({
    address: z.string().default('localhost:7233'),
    namespace: z.string().default('default'),
    taskQueue: z.string().default('aegis-agents'),
  }),
  database: z.object({
    url: z.string(),
  }),
  grpc: z.object({
    runtimeUrl: z.string().default('localhost:50051'),
  }),
  http: z.object({
    port: z.coerce.number().default(3000),
    host: z.string().default('0.0.0.0'),
  }),
  worker: z.object({
    maxConcurrentActivityTaskExecutions: z.coerce.number().default(100),
    maxConcurrentWorkflowTaskExecutions: z.coerce.number().default(100),
  }),
  logging: z.object({
    level: z.enum(['trace', 'debug', 'info', 'warn', 'error']).default('info'),
  }),
  nodeEnv: z.enum(['development', 'production', 'test']).default('development'),
});

export type Config = z.infer<typeof configSchema>;

function loadConfig(): Config {
  return configSchema.parse({
    temporal: {
      address: process.env.TEMPORAL_ADDRESS,
      namespace: process.env.TEMPORAL_NAMESPACE,
      taskQueue: process.env.TEMPORAL_TASK_QUEUE,
    },
    database: {
      url: process.env.DATABASE_URL,
    },
    grpc: {
      runtimeUrl: process.env.AEGIS_RUNTIME_GRPC_URL,
    },
    http: {
      port: process.env.HTTP_PORT,
      host: process.env.HTTP_HOST,
    },
    worker: {
      maxConcurrentActivityTaskExecutions: process.env.MAX_CONCURRENT_ACTIVITY_TASK_EXECUTIONS,
      maxConcurrentWorkflowTaskExecutions: process.env.MAX_CONCURRENT_WORKFLOW_TASK_EXECUTIONS,
    },
    logging: {
      level: process.env.LOG_LEVEL,
    },
    nodeEnv: process.env.NODE_ENV,
  });
}

export const config = loadConfig();
