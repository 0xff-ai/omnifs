CREATE TABLE attach_endpoint (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    tcp_port INTEGER NOT NULL CHECK (tcp_port BETWEEN 1 AND 65535)
) STRICT;
