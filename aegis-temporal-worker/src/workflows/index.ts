/**
 * Temporal Workflows
 * 
 * We export the Generic Interpreter Workflow which handles all
 * AEGIS workflow definitions dynamically.
 */

import { aegis_workflow } from './aegis-workflow.js';

// Export with the specific name that the Rust client invokes
const workflows = {
  'aegis-workflow': aegis_workflow,
};

export default workflows;

// Individual exports for discovery if needed (though default export covers it)
export { aegis_workflow };
