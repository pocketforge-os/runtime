# Production compositor stack pins

These are immutable upstream commits, not PocketForge forks. They define the W00
production-compositor integration baseline; W00 itself proves the client contract with
Weston's headless backend because it runs without a GPU on Ubuntu CI.

| Component | Upstream revision | Role |
|---|---|---|
| Gamescope 3.16.15 | `1faf7acd90f960b8e6c816bfea15f699b70527f9` | production compositor candidate |
| Mesa 25.1.7 | `581a4f7e70f1fb37d8640d707105b2e4cea99144` | Vulkan userspace driver baseline |
| Vulkan-Headers 1.4.321 | `2cd90f9d20df57eac214c148f3aed885372ddcfe` | Vulkan API headers |
| Vulkan-Loader 1.4.321 | `da8d2caad9341ca8c5a7c3deba217d7da50a7c24` | Vulkan loader |
| Weston 13.0.0 | `712cdc56ab7eab5f11f4934e04904a6d01a07733` | GPU-free headless proof compositor |

The hashes were resolved from the upstream tag refs. Gamescope is pinned but not
forked or exercised here: this host is compositor-neutral, and W00 makes no
production-compositor selection, GPU, driver, latency, or handheld/KMS claim.

