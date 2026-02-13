/**
 * Logger configuration using Pino
 */

import { pino } from 'pino';
import { config } from './config.js';

export const logger = pino({
  level: config.logging.level,
  transport:
    config.nodeEnv === 'development'
      ? {
          target: 'pino-pretty',
          options: {
            colorize: true,
            translateTime: 'HH:MM:ss Z',
            ignore: 'pid,hostname',
          },
        }
      : undefined,
});
