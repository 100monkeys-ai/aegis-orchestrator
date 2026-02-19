
# Running Rust Guardian on Ubuntu

This guide explains how to install and run `rust-guardian` to analyze the `aegis-orchestrator` codebase on Ubuntu.

## Prerequisites

- Rust toolchain (cargo) installed.
- `build-essential` package (for compiling dependencies).

## Installation

Install `rust-guardian` using cargo:

```bash
cargo install rust-guardian
```

Ensure that `$HOME/.cargo/bin` is in your `PATH`.

## Configuration

The project is already configured with:

- `guardian.yaml`: Main configuration file in the project root.
- `.guardianignore`: Exclustion patterns for files/directories to ignore.

## Running the Check

To run the analysis and print to stdout:

```bash
rust-guardian check
```

### Generating a Report File

To save the output to a file for review:

```bash
rust-guardian check -f agent > guardian_report.json
```
