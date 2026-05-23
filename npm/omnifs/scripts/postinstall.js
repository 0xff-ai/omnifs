"use strict";

const { resolveBinary } = require("./resolve-binary");

if (process.env.OMNIFS_SKIP_POSTINSTALL_CHECK === "1") {
  process.exit(0);
}

const resolved = resolveBinary();
if (!resolved.ok) {
  console.error(resolved.message);
  process.exit(1);
}
