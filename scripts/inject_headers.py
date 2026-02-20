import os
import re
import sys

# Mappings of file keywords or paths to their related ADRs
ADR_MAPPINGS = {
    "cortex/src/infrastructure/embedding_client.rs": "ADR-028: Embedding Model Selection",
    "cortex/src/application/cortex_pruner.rs": "ADR-029: Cortex Time-Decay Parameters",
    "orchestrator/core/src/infrastructure/runtime.rs": "ADR-027: Docker Runtime Implementation",
    "orchestrator/core/src/infrastructure/event_bus.rs": "ADR-030: Event Bus Architecture",
    "orchestrator/core/src/application/workflow_engine.rs": "ADR-031: Handlebars Template Engine",
    "orchestrator/core/src/domain/volume.rs": "ADR-032: Unified Storage via SeaweedFS",
    "orchestrator/tool_integration": "ADR-033: Orchestrator-Mediated MCP Tool Routing",
    "orchestrator/core/src/infrastructure/secrets": "ADR-034: OpenBao Secrets Management",
    "orchestrator/core/src/infrastructure/smcp": "ADR-035: SMCP Implementation",
    "orchestrator/core/src/infrastructure/nfs_gateway": "ADR-036: NFS Server Gateway Architecture",
    "storage/fsal": "ADR-036: NFS Server Gateway Architecture",
}

def determine_layer(path):
    path_lower = path.lower()
    if 'domain\\' in path_lower or 'domain/' in path_lower:
        return 'Domain Layer'
    elif 'application\\' in path_lower or 'application/' in path_lower:
        return 'Application Layer'
    elif 'infrastructure\\' in path_lower or 'infrastructure/' in path_lower:
        return 'Infrastructure Layer'
    elif 'presentation\\' in path_lower or 'presentation/' in path_lower:
        return 'Presentation Layer'
    elif 'cli\\' in path_lower or 'cli/' in path_lower:
        return 'Interface / Presentation Layer'
    elif 'swarm\\' in path_lower or 'swarm/' in path_lower:
        return 'Swarm Coordination Layer'
    elif 'cortex\\' in path_lower or 'cortex/' in path_lower:
        return 'Learning & Memory Layer'
    return 'Core System'

def determine_adrs(path_str):
    adrs = []
    path_norm = path_str.replace('\\', '/')
    for pattern, adr in ADR_MAPPINGS.items():
        if pattern in path_norm:
            adrs.append(adr)
    return adrs

def process_file(filepath):
    with open(filepath, 'r', encoding='utf-8') as f:
        content = f.read()

    lines = content.split('\n')
    out_lines = []
    
    # 1. Look for Copyright header
    copyright_present = False
    idx = 0
    while idx < len(lines):
        if lines[idx].startswith('// Copyright (c)') or lines[idx].startswith('// SPDX-License-Identifier'):
            copyright_present = True
            out_lines.append(lines[idx])
            idx += 1
        elif lines[idx].strip() == '' and copyright_present and idx < 2:
            out_lines.append(lines[idx])
            idx += 1
        else:
            break
            
    if not copyright_present:
        out_lines = [
            "// Copyright (c) 2026 100monkeys.ai",
            "// SPDX-License-Identifier: AGPL-3.0",
            ""
        ] + out_lines
        idx = 0 # reset because we didn't consume any copyright lines

    # 2. Look for existing doc comments (start with //!)
    doc_lines = []
    while idx < len(lines):
        if lines[idx].startswith('//!') or lines[idx].strip() == '':
            if lines[idx].strip() == '' and len(doc_lines) == 0:
                idx += 1
                continue
            doc_lines.append(lines[idx])
            idx += 1
        else:
            break

    # Strip trailing empty lines from doc_lines
    while doc_lines and doc_lines[-1].strip() == '':
        doc_lines.pop()
        
    # Check if doc_lines already has an Architecture section
    has_architecture = any('//! # Architecture' in line for line in doc_lines)
    
    filename = os.path.basename(filepath)
    module_name = filename.replace('.rs', '').replace('_', ' ').title()
    layer = determine_layer(filepath)
    adrs = determine_adrs(filepath)
    
    if not has_architecture:
        # If no doc comments at all, create basic ones
        if not doc_lines:
            doc_lines.append(f"//! {module_name}")
            doc_lines.append(f"//!")
            doc_lines.append(f"//! Provides {module_name.lower()} functionality for the system.")
            doc_lines.append(f"//!")
            doc_lines.append(f"//! # Architecture")
            doc_lines.append(f"//!")
            doc_lines.append(f"//! - **Layer:** {layer}")
            doc_lines.append(f"//! - **Purpose:** Implements {module_name.lower()}")
            if adrs:
                doc_lines.append(f"//! - **Related ADRs:** {', '.join(adrs)}")
        else:
            # Append Architecture section
            doc_lines.append(f"//!")
            doc_lines.append(f"//! # Architecture")
            doc_lines.append(f"//!")
            doc_lines.append(f"//! - **Layer:** {layer}")
            # Try to guess purpose from filename
            doc_lines.append(f"//! - **Purpose:** Implements internal responsibilities for {module_name.lower()}")
            if adrs:
                doc_lines.append(f"//! - **Related ADRs:** {', '.join(adrs)}")
    else:
        # It already has an Architecture section.
        # We can try to inject Related ADRs if not present
        if adrs:
            has_adrs = any('Related ADRs' in line for line in doc_lines)
            if not has_adrs:
                # find where Architecture section ends or file ends
                arch_idx = next(i for i, line in enumerate(doc_lines) if '//! # Architecture' in line)
                # insert after Layer / Purpose
                insert_idx = arch_idx + 1
                while insert_idx < len(doc_lines) and doc_lines[insert_idx].strip() != '//!' and doc_lines[insert_idx].startswith('//!-') == False and 'Related ADRs' not in doc_lines[insert_idx] and doc_lines[insert_idx].startswith('//! -'):
                    insert_idx += 1
                # Find the end of the bulleted list under Architecture
                while insert_idx < len(doc_lines) and doc_lines[insert_idx].startswith('//! -'):
                    insert_idx += 1
                doc_lines.insert(insert_idx, f"//! - **Related ADRs:** {', '.join(adrs)}")

    # Add the doc lines to out_lines
    out_lines.extend(doc_lines)
    if doc_lines:
        out_lines.append("")
        
    # Skip any empty lines before the actual code starts
    while idx < len(lines) and lines[idx].strip() == '':
        idx += 1
        
    # Append the rest of the code
    out_lines.extend(lines[idx:])
    
    new_content = '\n'.join(out_lines)
    
    if new_content != content:
        with open(filepath, 'w', encoding='utf-8') as f:
            f.write(new_content)
        print(f"Updated: {filepath}")

def main():
    repo_dir = r"c:/Users/travi/Documents/git_repos/aegis-greenfield/repos/aegis-orchestrator"
    
    for root, dirs, files in os.walk(repo_dir):
        # ignore node_modules, target, dist
        if 'node_modules' in dirs:
            dirs.remove('node_modules')
        if 'target' in dirs:
            dirs.remove('target')
        if 'dist' in dirs:
            dirs.remove('dist')
            
        for file in files:
            if file.endswith('.rs'):
                filepath = os.path.join(root, file)
                try:
                    process_file(filepath)
                except Exception as e:
                    print(f"Error processing {filepath}: {e}")

if __name__ == '__main__':
    main()
