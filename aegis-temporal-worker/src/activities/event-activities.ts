import { logger } from '../logger.js';

export interface PublishEventParams {
  event_type: string;
  execution_id: string;
  temporal_sequence_number: number;
  workflow_id?: string;
  state_name?: string;
  output?: any;
  error?: string;
  iteration_number?: number;
  final_blackboard?: any;
  artifacts?: string[];
  timestamp?: string;
}

/**
 * Publish a workflow event to the AEGIS Rust orchestrator backend.
 * Uses native fetch to send HTTP POST request to /v1/temporal-events.
 */
export async function publishEventActivity(params: PublishEventParams): Promise<void> {
  const orchestratorUrl = process.env.AEGIS_ORCHESTRATOR_URL || 'http://localhost:8000';
  const endpoint = `${orchestratorUrl}/v1/temporal-events`;

  logger.info(
    { event_type: params.event_type, execution_id: params.execution_id },
    'Publishing Temporal event to backend'
  );

  // Ensure timestamp exists
  const payload = {
    ...params,
    timestamp: params.timestamp || new Date().toISOString()
  };

  try {
    const response = await fetch(endpoint, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
      },
      body: JSON.stringify(payload),
    });

    if (!response.ok) {
      const errorText = await response.text();
      throw new Error(`Orchestrator returned ${response.status}: ${errorText}`);
    }
  } catch (error) {
    logger.error(
      { error, event_type: params.event_type, execution_id: params.execution_id },
      'Failed to publish event to orchestrator'
    );
    throw error;
  }
}
