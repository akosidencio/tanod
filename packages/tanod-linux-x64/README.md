# @tanod/linux-x64

The [Tanod](https://github.com/akosidencio/tanod) binary for Linux x64,
published so JavaScript projects can install Tanod with their package manager
instead of a separate image or download.

It is a static musl build with TLS, so the same file runs on glibc
distributions and on Alpine. You normally do not install this package
directly: `@tanod/next` lists it as an optional dependency, and
`tanod-next start` finds it. It also puts `tanod` on `node_modules/.bin`:

```bash
npx tanod version
npx tanod check --config tanod.yaml
```

The `bin/tanod` file is added by the release workflow from the tagged build;
it is not in the repository.
