// `tanod-next start`: run an app server and Tanod in front of it as one
// process tree, so a deployment is one container instead of an app, a proxy
// image and the glue between them.
//
// Nothing here is specific to Next.js except the default origin command.
// Any server that listens on a port works — `-- node server.js`, a Nuxt or
// SvelteKit build, anything — and the binary comes from a framework-neutral
// platform package, so this can move to a package of its own once it has
// proven itself beyond Next.
//
// The origin starts first and Tanod only once the origin's port accepts
// connections, so a platform health check on Tanod's port passes only when the
// whole thing can serve. Shutdown is the reverse: Tanod drains first, then the
// origin stops, so requests Tanod already admitted can finish. If either
// process dies on its own the other is stopped and the exit code is passed on,
// which is what a container platform needs to restart the pair.

import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { createRequire } from 'node:module';
import net from 'node:net';
import path from 'node:path';

import { TanodNextError } from './manifests.js';

const require = createRequire(import.meta.url);

export const START_USAGE = `tanod-next start — run the app and Tanod in front of it

USAGE
  tanod-next start [OPTIONS] [-- <origin command...>]

  The origin is any command that serves HTTP on a port. With none, runs
  \`next start\` from the current directory. Examples:
    Next standalone   -- node server.js
    Nuxt              -- node .output/server/index.mjs
    SvelteKit (node)  -- node build

OPTIONS
  --config <FILE>       Tanod configuration. Default: $TANOD_CONFIG, then
                        ./tanod.yaml (Tanod's own lookup).
  --origin-port <N>     Port the origin listens on, on 127.0.0.1; the config's
                        origin.upstreams must point here. Default:
                        $TANOD_ORIGIN_PORT, else 3000.
  --origin-timeout <S>  Seconds to wait for the origin to accept connections.
                        Default: 60.
  --tanod-bin <PATH>    The tanod binary. Default: $TANOD_BIN, then the
                        @tanod/linux-x64 package, then tanod on PATH.

ENVIRONMENT
  The origin gets PORT=<origin port> and HOSTNAME=127.0.0.1, which is what
  \`next start\` and a standalone server.js read. Tanod gets the environment
  unchanged, so its config can use \${PORT} for the public listener.
`;

const PLATFORM_PACKAGES = {
  'linux-x64': '@tanod/linux-x64',
};

/**
 * The tanod binary to run: explicit, `TANOD_BIN`, the platform package, then
 * `tanod` on `PATH`.
 */
export function resolveTanodBinary(explicit, env = process.env, resolve = require.resolve) {
  if (explicit) return explicit;
  if (env.TANOD_BIN) return env.TANOD_BIN;
  const pkg = PLATFORM_PACKAGES[`${process.platform}-${process.arch}`];
  if (pkg) {
    try {
      return resolve(`${pkg}/bin/tanod`);
    } catch {
      // Not installed (optional dependencies skipped, or another platform's
      // lockfile); fall through to PATH.
    }
  }
  return 'tanod';
}

export function parseStartArgs(argv, env = process.env) {
  const options = {
    config: null,
    originPort: Number(env.TANOD_ORIGIN_PORT || 3000),
    originTimeout: 60,
    tanodBin: null,
    origin: null,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const value = () => {
      const v = argv[i + 1];
      if (v === undefined) throw new TanodNextError(`${arg} needs a value`);
      i += 1;
      return v;
    };
    switch (arg) {
      case '--':
        options.origin = argv.slice(i + 1);
        i = argv.length;
        break;
      case '--config':
        options.config = value();
        break;
      case '--origin-port':
        options.originPort = Number(value());
        break;
      case '--origin-timeout':
        options.originTimeout = Number(value());
        break;
      case '--tanod-bin':
        options.tanodBin = value();
        break;
      default:
        throw new TanodNextError(`unknown option \`${arg}\` for start`);
    }
  }
  if (!Number.isInteger(options.originPort) || options.originPort < 1 || options.originPort > 65535) {
    throw new TanodNextError('--origin-port must be a TCP port');
  }
  if (!(options.originTimeout > 0)) {
    throw new TanodNextError('--origin-timeout must be a positive number of seconds');
  }
  if (options.origin && options.origin.length === 0) {
    throw new TanodNextError('nothing after `--`; give the origin command or drop the `--`');
  }
  return options;
}

/** The default origin: this project's own `next start`. */
function nextStartCommand(port, cwd) {
  let nextBin;
  try {
    nextBin = require.resolve('next/dist/bin/next', { paths: [cwd] });
  } catch {
    throw new TanodNextError(
      'could not find `next` in this project; run from the app directory, or pass the server ' +
        'command after `--` (for standalone output: -- node server.js)',
    );
  }
  return [process.execPath, nextBin, 'start', '-H', '127.0.0.1', '-p', String(port)];
}

function waitForPort(port, timeoutMs, originExited) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve, reject) => {
    const attempt = () => {
      if (originExited()) return;
      const socket = net.connect({ host: '127.0.0.1', port });
      socket.once('connect', () => {
        socket.destroy();
        resolve();
      });
      socket.once('error', () => {
        socket.destroy();
        if (Date.now() >= deadline) {
          reject(new TanodNextError(`the origin did not accept connections on 127.0.0.1:${port} within ${timeoutMs / 1000}s`));
        } else {
          setTimeout(attempt, 200);
        }
      });
    };
    attempt();
  });
}

/**
 * Run both processes until one exits or a signal arrives. Resolves with the
 * exit code for this process: 0 after a requested shutdown, otherwise the
 * code of whichever process stopped first.
 */
export function start(
  argv,
  {
    env = process.env,
    cwd = process.cwd(),
    log = (m) => process.stderr.write(`tanod-next: ${m}\n`),
    shutdownGraceMs = 30_000,
  } = {},
) {
  const options = parseStartArgs(argv, env);
  const originCommand = options.origin ?? nextStartCommand(options.originPort, cwd);
  const tanodBinary = resolveTanodBinary(options.tanodBin, env);
  if (path.isAbsolute(tanodBinary) && !existsSync(tanodBinary)) {
    throw new TanodNextError(`tanod binary ${tanodBinary} does not exist`);
  }
  const tanodArgs = ['run', ...(options.config ? ['--config', options.config] : [])];

  return new Promise((resolve) => {
    let origin = null;
    let tanod = null;
    let originExit = null;
    let tanodExit = null;
    let stopping = false;
    // The code to exit with, set by the first process that stopped on its own.
    let result = null;
    let killTimer = null;
    let finished = false;

    const running = (child, exit) => child !== null && exit === null;
    const codeOf = (exit) => exit.code ?? 128 + (signalNumber(exit.signal) ?? 15);

    const finish = () => {
      if (finished) return;
      if (running(origin, originExit) || running(tanod, tanodExit)) return;
      finished = true;
      if (killTimer) clearTimeout(killTimer);
      for (const signal of Object.keys(handlers)) process.removeListener(signal, handlers[signal]);
      resolve(result ?? 0);
    };

    const armKillTimer = () => {
      if (killTimer) return;
      killTimer = setTimeout(() => {
        log(`shutdown took longer than ${shutdownGraceMs / 1000}s; killing what is left`);
        if (running(tanod, tanodExit)) tanod.kill('SIGKILL');
        if (running(origin, originExit)) origin.kill('SIGKILL');
      }, shutdownGraceMs);
      killTimer.unref?.();
    };

    // Stop the pair, Tanod first so it can drain what it admitted; the origin
    // is stopped when Tanod has exited (see onTanodExit).
    const stop = (reason, code) => {
      if (stopping) return;
      stopping = true;
      if (code !== undefined) result = code;
      log(reason);
      if (running(tanod, tanodExit)) tanod.kill('SIGTERM');
      else if (running(origin, originExit)) origin.kill('SIGTERM');
      armKillTimer();
      finish();
    };

    const onOriginExit = (exit) => {
      if (originExit !== null) return;
      originExit = exit;
      if (!stopping) {
        stop(`origin exited (${exit.signal ?? exit.code}); stopping`, codeOf(exit) || 1);
      } else if (running(tanod, tanodExit)) {
        tanod.kill('SIGTERM');
      }
      finish();
    };

    const onTanodExit = (exit) => {
      if (tanodExit !== null) return;
      tanodExit = exit;
      if (!stopping) {
        stop(`tanod exited (${exit.signal ?? exit.code}); stopping the origin`, codeOf(exit) || 1);
      } else if (result === null && exit.code) {
        // A requested shutdown that tanod itself failed is still a failure.
        result = exit.code;
      }
      if (running(origin, originExit)) origin.kill('SIGTERM');
      finish();
    };

    const onSignal = (signal) => () => {
      if (stopping) {
        // A second signal: stop waiting for a graceful drain.
        log(`${signal} again; killing both`);
        if (running(tanod, tanodExit)) tanod.kill('SIGKILL');
        if (running(origin, originExit)) origin.kill('SIGKILL');
        return;
      }
      stop(`${signal} received; draining tanod, then stopping the origin`);
    };
    const handlers = {
      SIGTERM: onSignal('SIGTERM'),
      SIGINT: onSignal('SIGINT'),
      // Tanod reloads its config on SIGHUP; the origin has no use for it.
      SIGHUP: () => {
        if (running(tanod, tanodExit)) tanod.kill('SIGHUP');
      },
    };
    for (const signal of Object.keys(handlers)) process.on(signal, handlers[signal]);

    const [originBin, ...originArgs] = originCommand;
    log(`starting origin: ${originCommand.join(' ')} (127.0.0.1:${options.originPort})`);
    origin = spawn(originBin, originArgs, {
      cwd,
      stdio: 'inherit',
      env: { ...env, PORT: String(options.originPort), HOSTNAME: '127.0.0.1' },
    });
    origin.once('error', (error) => {
      log(`could not start the origin: ${error.message}`);
      onOriginExit({ code: 127, signal: null });
    });
    origin.once('exit', (code, signal) => onOriginExit({ code, signal }));

    waitForPort(options.originPort, options.originTimeout * 1000, () => originExit !== null)
      .then(() => {
        if (stopping) return;
        log(`origin is up; starting ${tanodBinary} ${tanodArgs.join(' ')}`);
        tanod = spawn(tanodBinary, tanodArgs, { cwd, stdio: 'inherit', env });
        tanod.once('error', (/** @type {NodeJS.ErrnoException} */ error) => {
          log(
            error.code === 'ENOENT'
              ? `tanod binary \`${tanodBinary}\` not found; install @tanod/next with optional ` +
                  'dependencies on linux-x64, or set TANOD_BIN'
              : `could not start tanod: ${error.message}`,
          );
          onTanodExit({ code: 127, signal: null });
        });
        tanod.once('exit', (code, signal) => onTanodExit({ code, signal }));
      })
      .catch((error) => stop(error.message, 1));
  });
}

function signalNumber(signal) {
  return { SIGHUP: 1, SIGINT: 2, SIGKILL: 9, SIGTERM: 15 }[signal];
}
