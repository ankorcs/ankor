/**
 * Rebuilds `@ankorcs/protocol` while `astro dev` runs, so a contract edit reaches the reference. The
 * package's build writes the AsyncAPI document and the `agent.yml` schema that the site reads, from
 * its schemas and the JSDoc in their source.
 */
import { execFile } from 'node:child_process';
import { createRequire } from 'node:module';
import path from 'node:path';
import { promisify } from 'node:util';
import type { Plugin } from 'vite';

const run = promisify(execFile);
const resolve = createRequire(import.meta.url).resolve;

/** The built files that the site reads. */
const OUTPUTS = ['asyncapi.yaml', 'agent-yml.schema.json'].map((name) => resolve(`@ankorcs/protocol/${name}`));

/** The workspace's package, two levels above its built files in `dist/`. */
const PACKAGE = path.dirname(path.dirname(OUTPUTS[0]!));

/** The sources the build reads: the contract and the generator of the document. */
const SOURCES = [path.join(PACKAGE, 'src'), path.join(PACKAGE, 'reference')];

export function watchReference(): Plugin {
  let queued: Promise<void> = Promise.resolve();
  let timer: NodeJS.Timeout | undefined;

  return {
    name: 'ankor-watch-reference',
    apply: 'serve',
    configureServer(server) {
      async function rebuild(): Promise<void> {
        try {
          await run('npm', ['run', 'build'], { cwd: PACKAGE });
          // The site reloads what reads the files, as it would for a file of its own.
          OUTPUTS.forEach((file) => server.watcher.emit('change', file));
          console.log('[reference] rebuilt @ankorcs/protocol');
        } catch (error: unknown) {
          const output = error as { stdout?: string; stderr?: string; message: string };

          console.error(`[reference] ${output.stderr || output.stdout || output.message}`);
        }
      }

      server.watcher.add(SOURCES);
      server.watcher.on('change', (file) => {
        if (!SOURCES.some((dir) => file.startsWith(`${dir}${path.sep}`))) {
          return;
        }

        clearTimeout(timer);
        // An editor's save can touch several files at once; one run covers them.
        timer = setTimeout(() => {
          queued = queued.then(rebuild);
        }, 200);
      });
    },
  };
}
