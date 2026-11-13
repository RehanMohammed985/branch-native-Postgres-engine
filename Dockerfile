# Banyan in one container: the daemon plus the Postgres 16 it runs per branch.
#
#   docker build -t banyan .
#   docker run -d --name banyan --device /dev/fuse --cap-add SYS_ADMIN \
#     --security-opt apparmor:unconfined \
#     -p 127.0.0.1:5432:5432 -p 127.0.0.1:8432:8432 -v banyan-data:/var/lib/banyan banyan
#
# FUSE needs /dev/fuse and CAP_SYS_ADMIN inside the container.

FROM rust:1-trixie AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p banyan -p banyan-bench

FROM postgres:16
RUN apt-get update \
 && apt-get install -y --no-install-recommends fuse3 \
 && rm -rf /var/lib/apt/lists/* \
 && mkdir -p /var/lib/banyan /mnt/banyan \
 && chown postgres:postgres /var/lib/banyan /mnt/banyan
COPY --from=build /src/target/release/banyan /src/target/release/banyan-bench /usr/local/bin/
ENV PATH=/usr/lib/postgresql/16/bin:$PATH
# Postgres refuses to run as root, so the daemon runs as the postgres user.
USER postgres
VOLUME /var/lib/banyan
EXPOSE 5432 8432
ENTRYPOINT ["banyan"]
# Inside the container the endpoints listen on all interfaces; publish them
# on 127.0.0.1 only (see the run command above): clients are not authenticated.
CMD ["serve", "--listen", "0.0.0.0:5432", "--api", "0.0.0.0:8432"]
