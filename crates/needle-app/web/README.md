# Needle control plane

React and TypeScript sources for the local Needle runtime UI. Vite produces
`dist/`, which `needle-app` embeds in the Rust binary; Node is not required in
production.

```text
npm ci
npm test
npm run lint
npm run build
npm run test:e2e:local
```
