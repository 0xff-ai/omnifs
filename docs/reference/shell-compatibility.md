---
title: "Shell compatibility matrix"
description: "Unix tools omnifs aims to support and the current validation caveat."
---

The design target is that omnifs paths behave like real files for standard Linux tools.

| Family | Tools |
|---|---|
| Read content | `cat`, `head`, `tail`, `less`, `more`, `xxd`, `hexdump`, `od`, `file` |
| Search and traversal | `grep`, `rg`, `find`, `fd` |
| Stat-based | `ls`, `du`, `wc`, `stat` |
| Copy and archive | `cp`, `mv`, `tar`, `rsync` |
| Compare and hash | `diff`, `cmp`, `md5sum`, `sha256sum`, `b3sum` |
| Inspection | `jq`, `yq`, `xmllint` |
| Editors | `vim`, `neovim`, `nano` |

Current automated validation does not prove every tool in this matrix. Treat this as the compatibility target and use smoke/FUSE tests plus manual shell traversal for provider path changes.
