# Oracle Bootstrap Provider

## Overview

`drasi-bootstrap-oracle` performs the initial Oracle table snapshot for Drasi queries before `drasi-source-oracle` begins streaming incremental LogMiner changes.

## Configuration

```yaml
bootstrap_provider:
  type: oracle
  host: localhost
  port: 1521
  service: FREEPDB1
  user: system
  password: secret
  tables:
    - HR.EMPLOYEES
```

## Notes

• The provider requires Oracle Instant Client at runtime (see the [Oracle source README](../../sources/oracle/README.md#prerequisites) for platform-specific install instructions).
• When the Oracle source supplies a bootstrap SCN, snapshots are read with `AS OF SCN` so bootstrap and streaming start from the same Oracle snapshot boundary.
• Element IDs are generated from discovered primary keys or configured `table_keys` overrides.
• Unqualified table names default to the configured Oracle user schema.
• Cypher node labels should match the Oracle table name portion of each configured table.
