---
on:
  workflow_dispatch:
    inputs:
      target:
        description: 'Target system'
        required: true
        type: string
imports:
  - ../agents/source-planner.md
engine:
  id: copilot
  model: claude-sonnet-4.5
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

# source-planner

Write a plan for a ${{ github.event.inputs.target }} source and save it to a file in my workspace so that I can edit it.
