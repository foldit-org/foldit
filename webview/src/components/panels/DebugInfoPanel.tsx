/**
 * SolidJS debug info panel component.
 */

import { Component, createMemo, createSignal } from 'solid-js';

// state imports
import { useBackendData } from '../../services/adapters';

// service imports
import { backend, BackendEnvironment } from '../../services/backend';

// util imports
import { getBrowserInfo, getPlatformInfo } from '../../utils/browserInfo';

// style imports
import '../../styles/panels/DebugInfoPanel.css';

const LEVEL_LABELS: Record<string, string> = {
  error: 'ERR',
  warn: 'WRN',
  info: 'INF',
  debug: 'DBG',
  trace: 'TRC',
};

const DebugInfoPanel: Component = () => {
  const browserInfo = getBrowserInfo();
  const platformInfo = getPlatformInfo();
  const fps = useBackendData(state => state.fps);
  const selectedCount = useBackendData(state => state.selectedCount);
  const log = useBackendData(state => state.log);
  const isWebview = backend.getEnvironment() === BackendEnvironment.WEBVIEW;
  const [copied, setCopied] = createSignal(false);

  const copyLog = () => {
    const text = log();
    if (!text) return;
    navigator.clipboard.writeText(text).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    });
  };

  // Highlight log lines (supports both Rust log format and Rosetta tracer format)
  const highlightedLog = createMemo(() => {
    const currentLog = log();
    if (!currentLog) return null;

    return currentLog.split('\n')
      .filter((line: string) => line.length > 0)
      .map((line: string) => {
        // Rust log format: "LEVEL target: message"
        const rustMatch = line.match(/^(ERROR|WARN|INFO|DEBUG|TRACE)\s+([\w:]+):\s+(.*)$/);
        if (rustMatch) {
          const level = rustMatch[1];
          const target = rustMatch[2];
          const message = rustMatch[3];
          const shortTarget = target.split('::').pop() || target;

          const levelKey = level.toLowerCase();
          const levelClass = `log-level-${levelKey}`;

          return (
            <div class={`log-line ${levelClass}`}>
              <span class={`log-badge ${levelClass}`}>{LEVEL_LABELS[levelKey]}</span>
              <span class="log-tag">{shortTarget}</span>
              <span class="log-msg">{message}</span>
            </div>
          );
        }

        // Rosetta tracer format: "channel.name: {thread_id} message"
        const tracerMatch = line.match(/^([a-zA-Z_.]+):\s+\{\d+\}\s+(.*)$/);
        if (tracerMatch) {
          const channel = tracerMatch[1];
          const message = tracerMatch[2];
          const channelParts = channel.split('.');
          const shortName = channelParts[channelParts.length - 1];

          const levelKey = channel === 'console' || channel.includes('.console')
            ? 'error'
            : channel.startsWith('game.') ? 'warn'
            : channel.startsWith('standalone.') ? 'info'
            : channel.startsWith('interactive.') ? 'debug'
            : 'debug';
          const channelClass = `log-level-${levelKey}`;

          return (
            <div class={`log-line ${channelClass}`}>
              <span class={`log-badge ${channelClass}`}>{LEVEL_LABELS[levelKey]}</span>
              <span class="log-tag">{shortName}</span>
              <span class="log-msg">{message}</span>
            </div>
          );
        }

        return <div class="log-line log-level-debug"><span class="log-msg">{line}</span></div>;
      });
  });

  return (
    <>
      {!isWebview && (
        <>
          <div class="debug-row">
            <span class="debug-label">Browser</span>
            <span class="debug-value">{browserInfo.browser} <span class="debug-detail">{browserInfo.browserVersion}</span></span>
          </div>
          <div class="debug-row">
            <span class="debug-label">Engine</span>
            <span class="debug-value">{browserInfo.engineFamily} <span class="debug-detail">{browserInfo.engineVersion}</span></span>
          </div>
        </>
      )}
      {isWebview && (
        <div class="debug-row">
          <span class="debug-label">Platform</span>
          <span class="debug-value">{platformInfo.os} <span class="debug-detail">{platformInfo.osVersion}</span></span>
        </div>
      )}
      <div class="debug-row debug-status-row">
        <span><span class="debug-label">FPS</span> <span class="debug-value">{fps().toFixed(1)}</span></span>
        <span><span class="debug-label">Selected</span> <span class="debug-value">{selectedCount()}</span></span>
        {log() && (
          <button class="log-copy-btn" onClick={copyLog} title="Copy log">
            {copied() ? 'Copied' : 'Copy Log'}
          </button>
        )}
      </div>
      {log() && (
        <div class="debug-log-container">
          {highlightedLog()}
        </div>
      )}
    </>
  );
};

export default DebugInfoPanel;
