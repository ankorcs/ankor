# Development

Every check and build runs in a container, on a workstation as in CI, so a workstation needs only
Docker.

| Toolchain                    | Runs                                                                                                                                                                                            |
|------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| [compose.yaml](compose.yaml) | The pinned Node and .NET SDK of [toolchain.Dockerfile](scripts/toolchain.Dockerfile), as `docker compose run --rm toolchain <command>`. Guides name the command alone, such as `npm run build`. |
| The Rust agent's scripts     | A container of their own, as the [agent's scripts](packages/agent/README.md#scripts) describe.                                                                                                  |

```sh
docker compose run --rm toolchain npm ci
docker compose run --rm toolchain npm run check
```

- The container writes `node_modules` and .NET's `bin/` and `obj/` into the checkout as Linux
  builds, so Node or .NET on the host cannot share the checkout.
- Run `npm ci` again after `package-lock.json` changes.
- [.npmrc](.npmrc) turns dependency lifecycle scripts off. Keep it that way.

## Layout

| Path                | What it is                                                                                                          | Guide                                                                              |
|---------------------|---------------------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------|
| `packages/protocol` | The wire contract, the controller's agent session, the container contract and `node-setup.sh`, `@ankorcs/protocol`. | [Protocol](packages/protocol/README.md)                                            |
| `packages/agent`    | The game node's agent, in Rust, and its image.                                                                      | [Agent](packages/agent/README.md)                                                  |
| `packages/plugin`   | The C# match plugin on SwiftlyS2, and the CS2 image.                                                                | [Plugin](packages/plugin/README.md), [CS2 image](packages/plugin/docker/README.md) |
| `docs`              | Operator and protocol documentation, and the site that serves it.                                                   | [Node docs](docs/index.md), [Docs site](docs/src/README.md)                        |
| `scripts`           | Workstation tooling for the node images and the plugin.                                                             | [Tooling](#tooling)                                                                |
| `brand`             | The logo and wordmark files the README and the docs site use.                                                       | [Brand](brand/README.md)                                                           |

## Checks

| Command                                                            | Checks                                                                                                                                                                                                                                 |
|--------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `docker compose run --rm toolchain npm run check`                  | `build`, `typecheck`, `lint` (ESLint and every Markdown link and anchor), `test`, `docs:build`, the plugin's `dotnet test`, a build of its headless host and `scripts/plugin.sh contract`, as [package.json](package.json) lists them. |
| `packages/agent/scripts/check.sh --docker`                         | The Rust agent, with its Docker suite. Its [README](packages/agent/README.md#checks) describes its opt-in suites and mutation testing.                                                                                                 |
| `packages/agent/scripts/protocol-sync.sh`                          | That the agent's generated contract matches `packages/protocol`.                                                                                                                                                                       |
| `docker run --rm -v "$PWD:/repo" -w /repo rhysd/actionlint:1.7.12` | The GitHub workflows, with shellcheck on their scripts.                                                                                                                                                                                |

CI runs the same checks except actionlint. [Workflows](.github/workflows/README.md) charts each run.

- A pull request runs the link check and the checks of each part it changes. **All checks** passes
  when every check passed or had nothing to check.
- A push to `master` runs the checks of the parts it changed and of the packages it releases.
- [ci-changes.sh](scripts/ci-changes.sh) holds the one list of the paths each part reads. When a
  package starts reading another path, add it there.
- A Markdown file changes only the docs site, except the protocol's README and CHANGELOG, which npm
  shows and ships.

## Change the contract

1. Edit the protocol as [Changing the protocol](packages/protocol/README.md#changing-the-protocol)
   describes.
2. Bump `version` in [package.json](packages/protocol/package.json) and add the release to the
   protocol's [CHANGELOG](packages/protocol/CHANGELOG.md).
3. Run `packages/agent/scripts/protocol-sync.sh --update`.
4. Fix the agent against the new contract and bump `version` in
   [Cargo.toml](packages/agent/crates/agent/Cargo.toml).
5. If the change touches the plugin's side, mirror it in the [plugin's copies](#plugin-copies) and
   bump `AnkorVersion`.
6. Run every [check](#checks).

### Plugin copies

The plugin cannot read `packages/protocol`, so `packages/plugin/core/Wire` copies the parts it uses.

| Protocol                                                                   | Plugin copy                                                     |
|----------------------------------------------------------------------------|-----------------------------------------------------------------|
| The container contract, in `container.ts`                                  | `Launch.cs`                                                     |
| The gameplay events and frames, in `game-events.ts` and `plugin-socket.ts` | `PluginToController.cs`, `PluginEventFrame.cs`, `Primitives.cs` |
| Limits the plugin keeps to, such as `MAX_TEAM_NAME_LENGTH`                 | `AgentToPlugin.cs`                                              |

`scripts/plugin.sh contract` compares the copied constants with the protocol's and checks the
replay fixture against the protocol's types. It does not compare the types themselves.

## Tooling

| Tool                                             | Use it for                                                                                                                                  |
|--------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------|
| [nodes.sh](scripts/nodes.sh)                     | Build the node images and copy them to nodes over SSH.                                                                                      |
| [plugin.sh](scripts/plugin.sh)                   | Regenerate the replay fixture, run coverage and mutation testing, check the plugin against the contract, and retrieve development captures. |
| [conformance.sh](scripts/conformance.sh)         | Run the agent's conformance suite against the plugin's headless image, in Docker.                                                           |
| [node-setup.sh](packages/protocol/node-setup.sh) | Install or update a game node. A controller gives operators the command.                                                                    |

Each script's `--help` lists its commands and options.

### Development nodes

```sh
scripts/nodes.sh deploy root@<node> [root@<node> ...]
```

- The images take the release names, so a copied build replaces the release tags on the node.
  `--image-prefix` names others, which a node runs only with `official_images_only: false`.
- `build` refuses uncommitted changes, untracked files included. `--dirty` builds anyway, tags
  the build `-dirty` and leaves its revision label empty.
- The images are linux/amd64. On an ARM workstation, build on an x86_64 node with
  `DOCKER_HOST=ssh://root@<node> scripts/nodes.sh deploy root@<node> [root@<other> ...]`.
- To move a node onto a copied agent image, run `node-setup.sh --update --no-pull` on it.
- Both copied images carry the version label. A node with `auto_update` on replaces them once
  their [line](docs/operations.md#images) has a newer release. If the registry already has the
  same version, the node replaces them with the registry's build on its next poll. To keep a copied
  build, turn `auto_update` off.

## Release node images

| Image         | Version                                                                                                                                                                    | Git tag        |
|---------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------|----------------|
| `ankor-agent` | `version` in [Cargo.toml](packages/agent/crates/agent/Cargo.toml). Its Dockerfile refuses to label an image with any other.                                                | `agent-vX.Y.Z` |
| `ankor-cs2`   | `AnkorVersion` in [Directory.Build.props](packages/plugin/Directory.Build.props), which also versions the plugin. Its Dockerfile refuses to label an image with any other. | `cs2-vX.Y.Z`   |

Both are published to `ghcr.io/ankorcs`, tagged `latest` for the newest release, then `X`, `X.Y` and
`X.Y.Z`. [image-publish.yml](.github/workflows/image-publish.yml) pushes `X.Y.Z` last, because the
agent looks for a rebuild under it.
[Images](docs/operations.md#images) says what changes a major. Builds on SwiftlyS2's newest beta
are tagged `beta` and `X.Y.Z-swiftlys2-A.B.C-beta.N`. [Workflows](.github/workflows/README.md)
describes each release workflow.

- A new major of SwiftlyS2 is bumped by hand.
- The SwiftlyS2 job acts as a GitHub App, because pushes made with `GITHUB_TOKEN` start no
  workflows. The App needs Contents and Pull requests write, and Actions, Checks and Commit statuses
  read. Its id goes in `BUMP_APP_ID` and its private key in `BUMP_APP_PRIVATE_KEY`.
- Make `ankor-agent` and `ankor-cs2` public in the organization's package settings, so that nodes
  pull without logging in.
- `cs2-beta.yml` deletes old versions of `ankor-cs2`, which needs this repository's **Admin** role
  under **Manage Actions access** in the package's settings.

To release:

1. Bump the version of each image that changed, after [Change the contract](#change-the-contract)
   for a contract change. The protocol and both images share their `major.minor`, so a new minor
   moves all three together, and a patch moves only the part that changed.
   [release-line.sh](scripts/release-line.sh) fails `ci.yml`, `release.yml` and `npm run check`
   when their minors differ.
2. Merge to `master`, and check that the release workflow tagged the commit.
3. Update a node with **Update agent** or **Update CS2 image** in the controller, as
   [Operating a node](docs/operations.md#update-the-agent) describes.
4. Check that the node reconnects with the new version, then
   [launch a test server](docs/operations.md#launch-a-test-server).

### npm

`release.yml` publishes `@ankorcs/protocol` when npm lacks the `version` in its `package.json`.

- The package ships `dist/`, whose comments link to the docs site, as the
  [protocol README](packages/protocol/README.md) describes.
- [npm-readme.mjs](scripts/npm-readme.mjs) rewrites the relative links of the README and CHANGELOG
  before publishing.
- The workflow publishes through npm's trusted publishing, so no npm token is stored. The package's
  trusted publisher names the organization `ankorcs`, the repository `ankor` and `release.yml`.
- Set the package's publishing access to require two-factor authentication and disallow tokens.

## Docs site

[Docs site](docs/src/README.md) describes the site.
[deploy-docs.yml](.github/workflows/deploy-docs.yml) deploys it, on each `release.yml` run or by
hand.

- GitHub Pages must be enabled with **GitHub Actions** as its source.
- Actions ignores a CNAME file, so set the custom domain in the Pages settings.

## Conventions

- Use the `ankor` prefix for project names, packages, environment variables and container labels.
- Add tooling to the relevant workstation script rather than creating another entry point.
- Use strict TypeScript and ESM. Keep dependencies on current stable major versions.
- Prefer a small module of our own over a dependency that only wraps an API.
- A package reads a sibling package's build output, as it reads the published package. Build the
  sibling first, as `npm run build` does for `@ankorcs/protocol`.
- Define each type once. A dependency on part of a class takes `Pick<Class, …>`, and wire payload
  types come from the contract through `z.infer`.
- Pass a collaborator whole. A callee that needs several members of one object takes that object,
  or an interface of the members it uses. Pass members apart only when no common type holds them,
  or in Rust when borrowing the whole object would conflict with a borrow already taken.
- Comments give a reason the code cannot show, in one line where possible. They describe the code
  as it is, with no change history.
- Wire keys are snake_case, and code reads them under those names. Locals, parameters and internal
  types stay camelCase. Destructure as `{ slot_id: slotId }`.
- Contract fields are required unless the value can be absent in the game, and nullable only when
  null means something. Schemas are plain `z.object`, so unknown keys are ignored.
- Gameplay values are bounded for competitive Counter-Strike. A value that a game setting governs
  may take that setting's whole range; any other about ten times its stock value.
- Agent and CS2 images update separately, so a contract change releases compatible builds of the
  affected packages. Controllers migrate their stored data.
- Log lines, agent progress, Steam errors and in-game chat are English diagnostics. A controller
  words what it shows its users.
- Test scheduling, state changes and recovery, not wiring.
- Tests that pin a wire shape spell its keys out by hand, so a schema change fails them.
- Source code does not exist only for tests. When a change removes the last source caller of a
  method, field or export, delete it and give the tests a lookup in their helpers.
