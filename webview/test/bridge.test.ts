import { vi, describe, it, expect, beforeEach } from 'vitest';
import type { FrontendState } from '../src/types';

// The bridge and the state channel both reach the host through the
// transport barrel. We mock the barrel so we can drive state deltas and
// inspect outbound command calls without a real wry/wasm impl.
const transportMock = vi.hoisted(() => {
  let pushed: ((delta: Partial<FrontendState>) => void) | null = null;
  return {
    // Captured by stateChannel's lazy subscribe; the test pushes deltas
    // through it to simulate the backend.
    push(delta: Partial<FrontendState>) {
      pushed?.(delta);
    },
    setSink(cb: (delta: Partial<FrontendState>) => void) {
      pushed = cb;
    },
    subscribe: vi.fn(),
    dispatchOp: vi.fn(),
    appCommand: vi.fn(),
    setSelection: vi.fn(),
    viewportInput: vi.fn(),
    request: vi.fn(),
    openSessionDialog: vi.fn(),
  };
});

vi.mock('../src/transport', () => ({
  subscribe: (cb: (delta: Partial<FrontendState>) => void) => {
    transportMock.setSink(cb);
    return () => {};
  },
  dispatchOp: transportMock.dispatchOp,
  appCommand: transportMock.appCommand,
  setSelection: transportMock.setSelection,
  viewportInput: transportMock.viewportInput,
  request: transportMock.request,
  openSessionDialog: transportMock.openSessionDialog,
}));

// stateChannel holds module-level mirror + listener state; reset modules
// between tests so each starts from the seed mirror with no listeners.
async function freshBridge() {
  vi.resetModules();
  const mod = await import('../src/bridge');
  return mod.createBridge();
}

beforeEach(() => {
  vi.clearAllMocks();
});

describe('PluginBridge.subscribe', () => {
  it('delivers a pushed delta', async () => {
    const bridge = await freshBridge();
    const seen: Partial<FrontendState>[] = [];
    bridge.subscribe((d) => seen.push(d));
    transportMock.push({ score: { value: 42, invalid: false, title: 'x' } });
    expect(seen).toHaveLength(1);
    expect(seen[0].score?.value).toBe(42);
  });

  it('section selector fires for a matching delta and skips others', async () => {
    const bridge = await freshBridge();
    const seen: Partial<FrontendState>[] = [];
    bridge.subscribe((d) => seen.push(d), ['panels']);
    transportMock.push({ panels: { open: ['plugin'], positions: [] } });
    transportMock.push({ score: { value: 1, invalid: false, title: '' } });
    expect(seen).toHaveLength(1);
    expect(seen[0].panels?.open).toEqual(['plugin']);
  });
});

describe('PluginBridge.snapshot', () => {
  it('reflects accumulated deltas and is a deep copy', async () => {
    const bridge = await freshBridge();
    // Subscribing opens the channel (wires the transport sink); deltas
    // then accumulate into the mirror that `snapshot` reads.
    bridge.subscribe(() => {});
    transportMock.push({ score: { value: 7, invalid: false, title: 'a' } });
    transportMock.push({ panels: { open: ['p'], positions: [] } });

    const snap = bridge.snapshot();
    expect(snap.score.value).toBe(7);
    expect(snap.panels.open).toEqual(['p']);

    // Mutating the snapshot must not touch the channel mirror.
    snap.score.value = 999;
    snap.panels.open.push('mutated');
    const snap2 = bridge.snapshot();
    expect(snap2.score.value).toBe(7);
    expect(snap2.panels.open).toEqual(['p']);
  });
});

describe('PluginBridge command forwarding', () => {
  it('forwards each command to the transport with the expected payload', async () => {
    const bridge = await freshBridge();

    bridge.dispatchOp({ op_id: 'foo' });
    expect(transportMock.dispatchOp).toHaveBeenCalledWith({ op_id: 'foo' });

    bridge.appCommand({ type: 'CloseSegment' });
    expect(transportMock.appCommand).toHaveBeenCalledWith({ type: 'CloseSegment' });

    bridge.setSelection([{ entity_id: 1, residues: [2, 3] }]);
    expect(transportMock.setSelection).toHaveBeenCalledWith([{ entity_id: 1, residues: [2, 3] }]);

    bridge.viewportInput({ kind: 'Scroll', delta: 5 });
    expect(transportMock.viewportInput).toHaveBeenCalledWith({ kind: 'Scroll', delta: 5 });

    bridge.openSessionDialog();
    expect(transportMock.openSessionDialog).toHaveBeenCalledTimes(1);
  });

  it('request forwards and resolves', async () => {
    const bridge = await freshBridge();
    transportMock.request.mockResolvedValueOnce({ ok: true });
    const result = await bridge.request('get_hotkey_text', { code: 'KeyW' });
    expect(transportMock.request).toHaveBeenCalledWith('get_hotkey_text', { code: 'KeyW' });
    expect(result).toEqual({ ok: true });
  });
});

describe('PluginBridge.contractVersion', () => {
  it('equals the stamped wire-contract version', async () => {
    const bridge = await freshBridge();
    const { BRIDGE_CONTRACT_VERSION } = await import('../src/generated/wire');
    expect(bridge.contractVersion).toBe(BRIDGE_CONTRACT_VERSION);
    expect(Number.isInteger(bridge.contractVersion)).toBe(true);
  });
});

describe('mountPluginPanel', () => {
  it('mounts behind an open shadow root and tears down via the plugin cleanup', async () => {
    vi.resetModules();
    const { createBridge, mountPluginPanel } = await import('../src/bridge');
    const bridge = createBridge();
    const host = document.createElement('div');

    const cleanup = vi.fn();
    const mountPanel = vi.fn((_panelId, shadow: ShadowRoot) => {
      shadow.appendChild(document.createElement('span'));
      return cleanup;
    });

    const teardown = mountPluginPanel(host, 'panelA', bridge.contractVersion, mountPanel, bridge);
    expect(host.shadowRoot).not.toBeNull();
    expect(host.shadowRoot?.mode).toBe('open');
    expect(host.shadowRoot?.childNodes).toHaveLength(1);
    expect(mountPanel).toHaveBeenCalledWith('panelA', host.shadowRoot, bridge);

    teardown();
    expect(cleanup).toHaveBeenCalledTimes(1);
    expect(host.shadowRoot?.childNodes).toHaveLength(0);
  });

  it('refuses on a contract-version mismatch', async () => {
    vi.resetModules();
    const { createBridge, mountPluginPanel } = await import('../src/bridge');
    const bridge = createBridge();
    const host = document.createElement('div');
    const mountPanel = vi.fn(() => vi.fn());

    expect(() =>
      mountPluginPanel(host, 'panelA', bridge.contractVersion + 1, mountPanel, bridge),
    ).toThrow();
    expect(mountPanel).not.toHaveBeenCalled();
    expect(host.shadowRoot).toBeNull();
  });
});
