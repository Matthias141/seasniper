import '@testing-library/jest-dom/vitest';
import { cleanup } from '@testing-library/react';
import { afterEach } from 'vitest';

// @testing-library/react's automatic cleanup only self-registers when it
// detects a Jest-like global afterEach — vitest exposes afterEach as a
// named import, not a global, unless `test.globals: true` is set (not
// set here, since this project doesn't otherwise rely on implicit
// globals). Without this, a component tree rendered in one test stays
// mounted into the next, which silently turns single-match DOM queries
// into multi-match failures across the whole suite.
afterEach(cleanup);
