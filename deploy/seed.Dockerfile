# The seed image: the app image plus a corpus to restore, which the restore
# job runs (deploy/README.md, "First deployment", step 6). npm run corpus:dump
# copies this file next to corpus.dump and manifest.json; build it there
# from the app image's digest:
#   az acr build --registry <acr> --platform linux/amd64 --image pghacker-seed:<corpus id> \
#     --build-arg APP_IMAGE=<acr>.azurecr.io/pghacker-mcp@sha256:<digest> .
ARG APP_IMAGE
FROM ${APP_IMAGE}
COPY corpus.dump manifest.json /seed/
CMD ["node", "scripts/corpus-restore.mjs"]
