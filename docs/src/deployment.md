# Deployment

Primary artifact: a **container image** containing the kwms binary plus the GDAL runtime libraries it links against. A sample multi-stage `Dockerfile` is part of the repo. Works on Kubernetes, Docker Compose, or plain Docker.

Standalone binaries are not a supported distribution channel in v1 (GDAL system-lib version skew is a recurring source of pain). Operators who want bare-metal installs can build from source.
