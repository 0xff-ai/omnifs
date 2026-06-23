# Chinook SQLite fixture for `omnifs dev`. Baked at build time; the fixture
# container seeds a host bind mount on first start.
FROM alpine:3.21
RUN apk add --no-cache curl \
    && mkdir -p /seed \
    && curl -fsSL \
        https://raw.githubusercontent.com/lerocha/chinook-database/master/ChinookDatabase/DataSources/Chinook_Sqlite.sqlite \
        -o /seed/test.db
COPY seed-entrypoint.sh /seed-entrypoint.sh
RUN chmod +x /seed-entrypoint.sh
ENTRYPOINT ["/seed-entrypoint.sh"]
