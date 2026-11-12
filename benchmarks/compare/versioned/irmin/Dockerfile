FROM ocaml/opam:debian-12-ocaml-5.2@sha256:28f1a3a456e6195187437da3b89e4218c1ac81f444c5475f9571e86accb9fc49
WORKDIR /home/opam/deps
COPY --chown=opam:opam irmin_bench.opam irmin_bench.opam.locked ./
RUN opam install --yes --deps-only --locked . && opam clean --all-switches --download-cache --logs
