# DB dev mount

Chinook SQLite fixture for the `default` and `full` dev profiles. The fixture image is built from this directory's `Dockerfile`; its container seeds `~/.omnifs-dev/fixtures/db/test.db` before the host-native daemon starts and remains alive until the dev session ends.
