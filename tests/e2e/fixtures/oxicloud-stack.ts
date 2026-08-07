import * as path from 'path';
import { loadEnv } from '../load-env';

/**
 * A running, isolated OxiCloud stack: one Postgres container + one OxiCloud
 * app container (Svelte SPA baked in, served by the Rust binary) wired
 * together on a private Docker network. Both publish random host ports, so
 * any number of stacks can run in parallel without collisions.
 */
export interface Stack {
  /** Host-mapped base URL of the app container, e.g. `http://localhost:49231`. */
  baseURL: string;
  /** Tear down the app, DB, and network. Safe to call once per stack. */
  stop(): Promise<void>;
}

const REPO_ROOT = path.join(__dirname, '../../..');
const SERVER_ENV_FILE = path.join(__dirname, '../../common/server.env');

// Match the DB image already pinned in tests/common/docker-compose.test.yml.
const POSTGRES_IMAGE = 'postgres:18.2-alpine3.23';
const APP_PORT = 8086;

/**
 * Start a fully isolated OxiCloud stack via Testcontainers.
 *
 * The app image is, in order of preference:
 *   1. `$OXICLOUD_IMAGE` — a prebuilt tag (recommended in CI: build once,
 *      reuse across workers).
 *   2. built on demand from the repo-root `Dockerfile` (cached by Docker
 *      layers; first run is slow, subsequent runs are fast).
 *
 * `testcontainers` is imported dynamically so the legacy webServer flow
 * (which never calls this function) doesn't pay its load cost.
 */
export async function startStack(): Promise<Stack> {
  const { GenericContainer, Network, Wait } = await import('testcontainers');
  const { PostgreSqlContainer } = await import('@testcontainers/postgresql');

  const serverEnv = loadEnv(SERVER_ENV_FILE);

  const network = await new Network().start();

  const postgres = await new PostgreSqlContainer(POSTGRES_IMAGE)
    .withDatabase('oxicloud_test')
    .withUsername('oxicloud_test')
    .withPassword('oxicloud_test')
    .withNetwork(network)
    .withNetworkAliases('db')
    .start();

  // Container-to-container DSN: the app reaches Postgres by its network alias,
  // not via the random host port. The stock postgres:18-alpine image already
  // ships pg_trgm, ltree, and citext, which the migrations require.
  const dbUri = 'postgres://oxicloud_test:oxicloud_test@db:5432/oxicloud_test';

  // VITE_E2E=1 keeps the `data-testid` tile hooks (stripped from release builds);
  // a prebuilt $OXICLOUD_IMAGE must have been built with the same arg.
  const baseImage = process.env.OXICLOUD_IMAGE
    ? new GenericContainer(process.env.OXICLOUD_IMAGE)
    : await GenericContainer.fromDockerfile(REPO_ROOT)
        // BuildKit (dockerode build version "2") is required for the Dockerfile's
        // `RUN --mount=type=cache` cargo caching; testcontainers defaults to the
        // classic builder, which would ignore the mounts. BUILDER=builder-cache
        // routes the runtime stage to that cache-mount builder so repeat builds
        // recompile only changed crates. (CI uses the default `builder` stage.)
        .withBuildkit()
        .withBuildArgs({ VITE_E2E: '1', BUILDER: 'builder-cache', BIN_DIR: '/app/bin' })
        .build('oxicloud-e2e:latest', {
          deleteOnExit: false,
        });

  const app = await baseImage
    .withNetwork(network)
    .withExposedPorts(APP_PORT)
    .withEnvironment({
      ...serverEnv,
      // Override the localhost:5433 DSN from server.env with the in-network one.
      OXICLOUD_DB_CONNECTION_STRING: dbUri,
      DATABASE_URL: dbUri,
      OXICLOUD_SERVER_PORT: String(APP_PORT),
      // The app defaults to binding 127.0.0.1, which a published container port
      // can't reach. Bind all interfaces so Testcontainers' mapped port works.
      OXICLOUD_SERVER_HOST: '0.0.0.0',
      // OPAQUE substrate off for the container E2E — same rationale as the
      // playwright.config.ts / playwright.coverage.config.ts overrides.
      // The Hurl suite exercises OPAQUE via server.env directly.
      OXICLOUD_AUTH_OPAQUE_MODE: 'off',
      OXICLOUD_AUTH_OPAQUE_SERVER_SETUP: '',
    })
    // /health is a fast liveness probe (no DB hit); 200 means the HTTP server
    // is up and sqlx migrations have completed.
    .withWaitStrategy(Wait.forHttp('/health', APP_PORT).forStatusCode(200))
    .withStartupTimeout(180_000)
    .start();

  const baseURL = `http://${app.getHost()}:${app.getMappedPort(APP_PORT)}`;

  return {
    baseURL,
    stop: async () => {
      await app.stop().catch(() => {});
      await postgres.stop().catch(() => {});
      await network.stop().catch(() => {});
    },
  };
}
