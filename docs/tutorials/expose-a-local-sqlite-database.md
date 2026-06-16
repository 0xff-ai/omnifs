---
title: Expose a local SQLite database
description: Mount a local SQLite file and read its schema and a sample, using the preopen model for local resources.
---

# Expose a local SQLite database

Goal: mount a local SQLite file and read its schema and a sample, using the preopen model for local resources.

1. `omnifs init db`
2. Point the config at your database file and grant the directory that holds it as a preopen.
3. `omnifs up`
4. `cat /omnifs/db/tables/artists/schema.sql`
5. `cat /omnifs/db/tables/artists/count.txt`
6. `jq '.[0]' /omnifs/db/tables/artists/sample.json`

The mount config looks like this:

```json
{
  "provider": "omnifs_provider_db.wasm",
  "mount": "db",
  "config": { "path": "/data/chinook.db" },
  "capabilities": {
    "preopened_paths": [ { "host": "/Users/you/data", "guest": "/data", "mode": "ro" } ]
  }
}
```

## Result

You read the schema, the row count, and a bounded sample of rows, each as its own file. The provider reached only the preopened directory, never the network. Note the disclosure caveat: read-only means the provider does not write, not that the data is non-sensitive. The sample returns real rows, so grant the preopen with care.
