---
on:
  slash_command:
    name: implement-source
    events: [pull_request, pull_request_comment]
imports:
  - ../agents/source-plan-executor.md
model: gpt-5.2-codex 
engine:
  id: copilot
permissions:
  copilot-requests: write
  contents: read
  issues: read
  pull-requests: read
tools:
  web-fetch:
  web-search:
  github:
safe-outputs:
  create-pull-request:
    draft: true
    expires: 14d
---

# source-implementor

Implement the plan to create a new source as specified in the planning PR.

Context: "${{ steps.sanitized.outputs.text }}"