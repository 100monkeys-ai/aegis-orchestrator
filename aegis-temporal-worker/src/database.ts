/**
 * PostgreSQL Database Client
 * Manages workflow definition persistence for multi-worker coordination
 */

import pkg from 'pg';
const { Pool } = pkg;
import { config } from './config.js';
import { logger } from './logger.js';
import type { TemporalWorkflowDefinition } from './types.js';

export class Database {
  private pool: pkg.Pool;

  constructor() {
    this.pool = new Pool({
      connectionString: config.database.url,
      max: 20,
      idleTimeoutMillis: 30000,
      connectionTimeoutMillis: 2000,
    });

    this.pool.on('error', (err) => {
      logger.error({ err }, 'Unexpected database error');
    });
  }

  async connect(): Promise<void> {
    try {
      const client = await this.pool.connect();
      await client.query('SELECT NOW()');
      client.release();
      logger.info('Database connected successfully');
    } catch (error) {
      logger.error({ error }, 'Failed to connect to database');
      throw error;
    }
  }

  async disconnect(): Promise<void> {
    await this.pool.end();
    logger.info('Database connection closed');
  }

  /**
   * Save workflow definition to database
   * Used for multi-worker coordination (all workers can load all definitions)
   */
  async saveWorkflowDefinition(definition: TemporalWorkflowDefinition): Promise<void> {
    const query = `
      INSERT INTO workflow_definitions (workflow_id, name, definition, registered_at, definition_hash)
      VALUES ($1, $2, $3, NOW(), $4)
      ON CONFLICT (workflow_id) 
      DO UPDATE SET 
        definition = EXCLUDED.definition,
        registered_at = NOW(),
        definition_hash = EXCLUDED.definition_hash
    `;

    const definitionHash = this.hashDefinition(definition);

    try {
      await this.pool.query(query, [
        definition.workflow_id,
        definition.name,
        JSON.stringify(definition),
        definitionHash,
      ]);
      logger.info({ workflow_id: definition.workflow_id, name: definition.name }, 'Workflow definition saved to database');
    } catch (error) {
      logger.error({ error, workflow_id: definition.workflow_id }, 'Failed to save workflow definition');
      throw error;
    }
  }

  /**
   * Load workflow definition by ID
   */
  async getWorkflowDefinition(workflowId: string): Promise<TemporalWorkflowDefinition | null> {
    const query = 'SELECT definition FROM workflow_definitions WHERE workflow_id = $1';

    try {
      const result = await this.pool.query(query, [workflowId]);
      if (result.rows.length === 0) {
        return null;
      }
      return result.rows[0].definition as TemporalWorkflowDefinition;
    } catch (error) {
      logger.error({ error, workflow_id: workflowId }, 'Failed to load workflow definition');
      throw error;
    }
  }

  /**
   * Load workflow definition by name
   */
  async getWorkflowDefinitionByName(name: string): Promise<TemporalWorkflowDefinition | null> {
    const query = 'SELECT definition FROM workflow_definitions WHERE name = $1';

    try {
      const result = await this.pool.query(query, [name]);
      if (result.rows.length === 0) {
        return null;
      }
      return result.rows[0].definition as TemporalWorkflowDefinition;
    } catch (error) {
      logger.error({ error, name }, 'Failed to load workflow definition by name');
      throw error;
    }
  }

  /**
   * Load all workflow definitions
   * Called on worker startup to register all workflows
   */
  async getAllWorkflowDefinitions(): Promise<TemporalWorkflowDefinition[]> {
    const query = 'SELECT definition FROM workflow_definitions ORDER BY registered_at DESC';

    try {
      const result = await this.pool.query(query);
      return result.rows.map((row) => row.definition as TemporalWorkflowDefinition);
    } catch (error) {
      logger.error({ error }, 'Failed to load all workflow definitions');
      throw error;
    }
  }

  /**
   * Delete workflow definition
   */
  async deleteWorkflowDefinition(workflowId: string): Promise<void> {
    const query = 'DELETE FROM workflow_definitions WHERE workflow_id = $1';

    try {
      await this.pool.query(query, [workflowId]);
      logger.info({ workflow_id: workflowId }, 'Workflow definition deleted');
    } catch (error) {
      logger.error({ error, workflow_id: workflowId }, 'Failed to delete workflow definition');
      throw error;
    }
  }

  /**
   * Simple hash function for definition versioning
   */
  private hashDefinition(definition: TemporalWorkflowDefinition): string {
    const str = JSON.stringify(definition);
    let hash = 0;
    for (let i = 0; i < str.length; i++) {
      const char = str.charCodeAt(i);
      hash = (hash << 5) - hash + char;
      hash = hash & hash; // Convert to 32-bit integer
    }
    return hash.toString(36);
  }
}

// Singleton instance
export const database = new Database();
