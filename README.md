# Rinha de Backend 2026 - IVF

Implementacao para a [Rinha de Backend 2026](https://github.com/zanfranceschi/rinha-de-backend-2026) com API em Rust, load balancer custom em C, busca vetorial IVF quantizada com SIMD AVX2 no hot path, comunicacao LB <-> API via Unix domain socket e reactor `epoll` single-thread por API.

O IVF reproduz o k-NN exato do labeling oficial em microssegundos contra o dataset (3M vetores).

## Stack

| Camada | Tecnologia | Por que |
| --- | --- | --- |
| Load balancer | C + `epoll`, UDS upstream, `TCP_NODELAY`, `TCP_QUICKACK`, `TCP_DEFER_ACCEPT` | proxy TCP simples, baixa latencia |
| API x2 | Rust + reactor `epoll` (mio) single-thread, listener TCP **e** Unix socket | sem thread-per-connection, keep-alive HTTP, parsing direto de bytes |
| Busca vetorial | Rust + IVF quantizado, AVX2 SIMD em centroides e clusters | k-NN aproximado com refinamento que reproduz o k-NN exato do labeling |
| Build | `target-cpu=x86-64-v3` (Haswell) | habilita AVX2/FMA por padrao |
| Dataset runtime | arquivos binarios em memoria | evita parse de JSON no startup e no request path |

## Documentacao

- [Documentacao tecnica](docs/tecnica.md)
