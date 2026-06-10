---
on:
  slash_command:
    name: implement-source
    events: [pull_request, pull_request_comment]
imports:
  - ../agents/source-plan-executor.md
engine:
  id: copilot
  model: gpt-5.2-codex 
permissions:
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
    expires: 14
---

# source-implementor

Implement the plan to create a new source as specified in the planning PR.

Context: "${{ needs.activation.outputs.text }}"
