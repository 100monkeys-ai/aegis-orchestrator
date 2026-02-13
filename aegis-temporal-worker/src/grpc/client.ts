/**
 * gRPC Client for AEGIS Runtime
 * Calls back to Rust ExecutionService, ValidationService, CortexService
 */

import * as grpc from '@grpc/grpc-js';
import * as protoLoader from '@grpc/proto-loader';
import { config } from '../config.js';
import { logger } from '../logger.js';
import type { 
  ExecuteAgentRequest,
  ExecutionEvent,
  ExecuteSystemCommandRequest,
  ExecuteSystemCommandResponse,
  ValidateRequest,
  ValidateResponse,
  QueryCortexRequest,
  QueryCortexResponse,
  StoreCortexPatternRequest,
  StoreCortexPatternResponse
} from '../types.js';

// Load protobuf definition
const PROTO_PATH = '../../proto/aegis_runtime.proto';

const packageDefinition = protoLoader.loadSync(PROTO_PATH, {
  keepCase: true,
  longs: String,
  enums: String,
  defaults: true,
  oneofs: true,
});

const aegisProto = grpc.loadPackageDefinition(packageDefinition) as any;

// Create gRPC client
class AegisRuntimeClient {
  private client: any;

  constructor(serverAddress: string) {
    this.client = new aegisProto.aegis_runtime.AegisRuntime(
      serverAddress,
      grpc.credentials.createInsecure()
    );
    logger.info({ server_address: serverAddress }, 'gRPC client initialized');
  }

  /**
   * Execute an agent (streaming response for real-time events)
   */
  async executeAgent(request: ExecuteAgentRequest): Promise<ExecutionEvent[]> {
    return new Promise((resolve, reject) => {
      const events: ExecutionEvent[] = [];

      const call = this.client.ExecuteAgent(request);

      call.on('data', (event: ExecutionEvent) => {
        logger.debug({ event_type: event.event_type }, 'Received execution event');
        events.push(event);
      });

      call.on('end', () => {
        logger.info({ event_count: events.length }, 'Agent execution completed');
        resolve(events);
      });

      call.on('error', (error: Error) => {
        logger.error({ error }, 'Agent execution failed');
        reject(error);
      });
    });
  }

  /**
   * Execute a system command
   */
  async executeSystemCommand(request: ExecuteSystemCommandRequest): Promise<ExecuteSystemCommandResponse> {
    return new Promise((resolve, reject) => {
      this.client.ExecuteSystemCommand(request, (error: Error | null, response: ExecuteSystemCommandResponse) => {
        if (error) {
          logger.error({ error }, 'System command execution failed');
          reject(error);
        } else {
          logger.info({ exit_code: response.exit_code }, 'System command completed');
          resolve(response);
        }
      });
    });
  }

  /**
   * Validate output with judge agents
   */
  async validateWithJudges(request: ValidateRequest): Promise<ValidateResponse> {
    return new Promise((resolve, reject) => {
      this.client.ValidateWithJudges(request, (error: Error | null, response: ValidateResponse) => {
        if (error) {
          logger.error({ error }, 'Validation with judges failed');
          reject(error);
        } else {
          logger.info({ final_score: response.final_score, confidence: response.confidence }, 'Validation completed');
          resolve(response);
        }
      });
    });
  }

  /**
   * Query Cortex for patterns matching an error
   */
  async queryCortexPatterns(request: QueryCortexRequest): Promise<QueryCortexResponse> {
    return new Promise((resolve, reject) => {
      this.client.QueryCortexPatterns(request, (error: Error | null, response: QueryCortexResponse) => {
        if (error) {
          logger.error({ error }, 'Cortex pattern query failed');
          reject(error);
        } else {
          logger.info({ pattern_count: response.patterns.length }, 'Cortex patterns retrieved');
          resolve(response);
        }
      });
    });
  }

  /**
   * Store a new pattern in Cortex
   */
  async storeCortexPattern(request: StoreCortexPatternRequest): Promise<StoreCortexPatternResponse> {
    return new Promise((resolve, reject) => {
      this.client.StoreCortexPattern(request, (error: Error | null, response: StoreCortexPatternResponse) => {
        if (error) {
          logger.error({ error }, 'Cortex pattern storage failed');
          reject(error);
        } else {
          logger.info({ pattern_id: response.pattern_id }, 'Cortex pattern stored');
          resolve(response);
        }
      });
    });
  }

  /**
   * Close the gRPC connection
   */
  close(): void {
    this.client.close();
    logger.info('gRPC client closed');
  }
}

// Singleton instance
export const aegisRuntimeClient = new AegisRuntimeClient(config.grpc.runtimeUrl);

// Export for testing
export { AegisRuntimeClient };
