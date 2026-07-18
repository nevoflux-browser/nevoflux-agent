---
name: reader
description: "Read-only code and file analysis"
mode: agent
allowed_tools:
  - "read"
  - "glob"
  - "grep"
max_iterations: 10
---

reader — a read-only code analysis agent. Inspects files and reports what is
there, never writing.
