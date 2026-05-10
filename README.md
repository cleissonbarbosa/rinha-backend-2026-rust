# Rinha de Backend 2026 - IVF

Implementacao para a [Rinha de Backend 2026](https://github.com/zanfranceschi/rinha-de-backend-2026) com API em Rust, load balancer custom em C, busca vetorial IVF quantizada com SIMD AVX2 no hot path, comunicacao LB <-> API via Unix domain socket e runtime assíncrono híbrido por API.

O IVF reproduz o k-NN exato do labeling oficial em microssegundos contra o dataset (3M vetores).

## Stack

| Camada | Tecnologia | Por que |
| --- | --- | --- |
| Load balancer | C + `epoll`, UDS upstream, `TCP_NODELAY`, `TCP_QUICKACK`, `TCP_DEFER_ACCEPT` | proxy TCP simples, baixa latencia |
| API x2 | Rust + `tokio-uring` no TCP e Tokio no UDS | `io_uring` no listener TCP, fallback compatível no socket Unix, sem thread-per-connection, keep-alive HTTP, parsing direto de bytes |
| Busca vetorial | Rust + IVF quantizado, AVX2 SIMD em centroides e clusters | k-NN aproximado com refinamento que reproduz o k-NN exato do labeling |
| Build | `target-cpu=x86-64-v3` (Haswell) | habilita AVX2/FMA por padrao |
| Dataset runtime | `mmap` para `.bin` e IVF | evita cópia dos bins no startup e mantém parse fora do request path |

## Documentacao

- [Documentacao tecnica](docs/tecnica.md)
