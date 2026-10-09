# PostgreSQL 17 with the extensions pghacker-mcp needs: Apache AGE (graph),
# pgvector (semantic search) and pg_textsearch (BM25). pg_trgm ships in contrib.
# The base image already trusts the PGDG repository, which builds all three.
FROM postgres:17-bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        "postgresql-17-age=1.7.*" \
        "postgresql-17-pgvector=0.8.*" \
        "postgresql-17-pg-textsearch=1.5.*" \
    && rm -rf /var/lib/apt/lists/*

# AGE and pg_textsearch must be preloaded.
CMD ["postgres", "-c", "shared_preload_libraries=age,pg_textsearch", "-c", "max_connections=200"]
