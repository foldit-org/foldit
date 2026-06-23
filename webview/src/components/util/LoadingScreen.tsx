/**
 * LoadingScreen Component
 * 
 * SolidJS loading screen component.
 */

import { Component, createSignal, createMemo, Show } from 'solid-js';
import logo_simple from '../../assets/logo_simple.svg';
import { useFrontendState } from '../../hooks/state';
import type { AppScreen } from '../../hooks/state';
import loading_tips from '../../data/loading_tips.json';

const LOADING_SCREENS: AppScreen[] = ['DOWNLOADING', 'INITIALIZING', 'LOADING_SESSION'];

// Simple CSS spinner component
const SpinnerCircularFixed: Component<{ color?: string; size?: number }> = (props) => {
  const { color = 'white', size = 30 } = props;

  return (
    <div
      style={{
        width: `${size}px`,
        height: `${size}px`,
        border: `2px solid ${color}`,
        'border-top': '2px solid transparent',
        'border-radius': '50%',
        animation: 'spin 1s linear infinite'
      }}
    />
  );
};

const LoadingScreen: Component = () => {
  const { appScreen, downloadProgress } = useFrontendState();

  // Random loading tip (initialize once)
  const [loadingTip] = createSignal(
    loading_tips[Math.floor(Math.random() * loading_tips.length)]
  );

  const statusString = createMemo(() => {
    switch (appScreen()) {
      case 'DOWNLOADING':
        return 'Downloading...';
      case 'INITIALIZING':
        return 'Initializing...';
      case 'LOADING_SESSION':
        return 'Loading Puzzle';
      default:
        return '';
    }
  });

  const shouldShow = createMemo(() => LOADING_SCREENS.includes(appScreen()));
  const shouldShowProgressBar = createMemo(() =>
    appScreen() === 'DOWNLOADING'
  );
  const shouldShowSpinner = createMemo(() =>
    appScreen() === 'INITIALIZING' || appScreen() === 'LOADING_SESSION'
  );

  return (
    <Show when={shouldShow()}>
      <div class="flex flex-col min-h-screen bg-black">
        <div class="flex-grow flex items-start justify-center pt-[30vh]">
          <img src={logo_simple} alt="logo" />
        </div>
        <div class="flex flex-col items-center justify-between pb-[5vh] space-y-4 text-white">
          <div class="flex flex-row items-center justify-center text-md">
            <p class="font-bold mr-2">Tip:</p>
            <p class="italic">{loadingTip()}</p>
          </div>
          <div class="text-lg pt-[10vh]">{statusString()}</div>

          <Show when={shouldShowProgressBar()}>
            <div class="w-64 h-6 bg-transparent border-2 border-white box-border">
              <div
                class="h-full bg-white transition-all duration-300 ease-out"
                style={{ width: `${downloadProgress()}%` }}
              />
            </div>
            <div class="text-white text-sm">
              {downloadProgress().toFixed(2)}%
            </div>
          </Show>

          <Show when={shouldShowSpinner()}>
            <SpinnerCircularFixed color="white" size={30} />
          </Show>
        </div>
      </div>
    </Show>
  );
};

export default LoadingScreen;
