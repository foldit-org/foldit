import { Component, createSignal, createMemo, Show } from 'solid-js';
import logo_simple from '../../assets/logo_simple.svg';
import '../../styles/util/ErrorScreen.css';
import { useBackendData, useFrontendState } from '../../services/adapters';

const ErrorScreen: Component = () => {
  const { backendErrorMessage } = useBackendData();
  const { appScreen } = useFrontendState();

  const [showDetails, setShowDetails] = createSignal(false);

  const isSafari = createMemo(() => /^((?!chrome|android).)*safari/i.test(navigator.userAgent));
  const isLinux = createMemo(() => /Linux/i.test(navigator.userAgent));
  const shouldShow = createMemo(() => appScreen() === 'BACKEND_ERROR');

  return (
    <Show when={shouldShow()}>
      <div class="error-page">
        <img
          class="flex flex-row mb-[5%]"
          src={logo_simple}
          alt="logo"
        />

        <Show when={isSafari()}>
          <div class="warning-message">
            Safari is known to have issues with WebAssembly applications. We recommend using another browser for the best experience.
          </div>
        </Show>

        <Show when={isLinux()}>
          <div class="warning-message">
            Linux support is experimental. Some features may not work correctly.
          </div>
        </Show>

        <div class="error-block">
          <p class="font-bold mb-2">
            An error has occurred in the Foldit WebAssembly module. Please refresh your browser window to keep playing.
          </p>
          <button
            onClick={() => setShowDetails(!showDetails())}
            class="underline cursor-pointer text-sm"
          >
            {showDetails() ? 'Hide details' : 'Show details'}
          </button>

          <Show when={showDetails()}>
            <pre class="whitespace-pre-wrap break-words mt-2 text-sm">
              {backendErrorMessage()}
            </pre>
          </Show>
        </div>
      </div>
    </Show>
  );
};

export default ErrorScreen;
