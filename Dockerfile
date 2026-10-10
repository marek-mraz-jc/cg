# Reproducible build: GCC 14 on Debian 12 (glibc 2.36). LadybugDB's C++ needs GCC >= 13.
# libstdc++ is linked statically, so the binary runs on any glibc >= 2.36 without GCC 14.
#   docker build --output dist .          -> dist/cg
FROM gcc:14-bookworm AS build
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal -c clippy
ENV PATH=/root/.cargo/bin:$PATH
# A directory with only libstdc++.a: the linker takes the static archive over the shared one.
RUN mkdir /static && cp /usr/local/lib64/libstdc++.a /static/
ENV RUSTFLAGS="-L native=/static"
WORKDIR /src
COPY . .
RUN cargo build --release --locked \
 && cargo test --release --locked \
 && cargo clippy --release --locked --all-targets -- -D warnings

FROM scratch
COPY --from=build /src/target/release/cg /cg
