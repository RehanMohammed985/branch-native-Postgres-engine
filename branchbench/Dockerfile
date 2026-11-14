# The BranchBench engine (github.com/ElaineAng/db-fork) next to a Banyan
# daemon, in one container. Built on the image from the repository root:
#
#   docker build -t banyan .
#   docker build -t banyan-branchbench branchbench
#
# The engine is fetched at a pinned commit and is not part of this repository.
ARG BANYAN_IMAGE=banyan
FROM ghcr.io/astral-sh/uv:0.9 AS uv
FROM ${BANYAN_IMAGE}
USER root
RUN apt-get update \
 && apt-get install -y --no-install-recommends git ca-certificates python3 procps \
 && rm -rf /var/lib/apt/lists/*
COPY --from=uv /uv /usr/local/bin/uv
ARG DBFORK_COMMIT=275c459bfe27d88c048dff1240f922731c7cacdd
RUN git clone https://github.com/ElaineAng/db-fork.git /opt/db-fork \
 && git -C /opt/db-fork checkout -q ${DBFORK_COMMIT} \
 && mkdir -p /out && chown -R postgres:postgres /opt/db-fork /out
USER postgres
WORKDIR /opt/db-fork
RUN uv sync --frozen
ENTRYPOINT ["bash"]
