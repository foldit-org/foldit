/**
 * TextBubble Component
 * 
 * SolidJS text bubble component with translation support and dragging.
 */

import { Component, Show, For, createSignal, createMemo, createEffect, onMount } from 'solid-js';
import '../../styles/widgets/TextBubble.css';
import { useUI } from '../../services/adapters';
import { TextBubbleButton } from '../../models/UI';
import { useTranslation } from '../../hooks/useTranslation';
import { calculateTargetCoords } from '../../utils/textBubbleUtils';
import { CalloutBubble } from '../util/CalloutBubble';
import Button from '../util/Button';
import ImageFromFS from '../util/ImageFromFS';
import { appCommand } from '../../adapter';

const TextBubble: Component = () => {
  const textBubble = useUI(state => state.textBubble);
  const textBubblePosition = useUI(state => state.textBubblePosition);
  const setTextBubblePosition = useUI(state => state.setTextBubblePosition);
  const hintsVisible = useUI(state => state.hintsVisible);

  const { translateText, needsTranslation } = useTranslation();

  const [translatedText, setTranslatedText] = createSignal<string>('');
  const [translatedButtons, setTranslatedButtons] = createSignal<TextBubbleButton[]>([]);
  const [isTranslationReady, setIsTranslationReady] = createSignal<boolean>(false);
  const [isDragging, setIsDragging] = createSignal(false);
  const [dragOffset, setDragOffset] = createSignal({ x: 0, y: 0 });

  let bubbleRef: HTMLDivElement | undefined;

  // Tier-1: anchor pointing (`point_to`) is unimplemented in the Rust
  // payload, so the tail is always suppressed. The createMemo here keeps
  // the call-site stable for when Tier-3 wires up anchor resolution.
  const showTail = createMemo(() => {
    const bubble = textBubble();
    return !!bubble?.target;
  });

  const tailCoords = createMemo(() => {
    if (!showTail() || !textBubble()?.target) return null;
    const targetCoords = calculateTargetCoords(textBubble()!.target);
    if (!targetCoords) return null;
    return { startX: targetCoords.x, startY: targetCoords.y };
  });

  // Translate content when textBubble changes.
  createEffect(async () => {
    const bubble = textBubble();
    if (!bubble) {
      setTranslatedText('');
      setTranslatedButtons([]);
      setIsTranslationReady(false);
      return;
    }

    if (needsTranslation) {
      setIsTranslationReady(false);
      const translated = await translateText(bubble.text);
      setTranslatedText(translated);

      const translatedBtns = await Promise.all(
        bubble.buttons.map(async (button) => ({
          ...button,
          text: await translateText(button.text)
        }))
      );
      setTranslatedButtons(translatedBtns);
    } else {
      setTranslatedText(bubble.text);
      setTranslatedButtons(bubble.buttons);
    }

    setIsTranslationReady(true);
  });

  const color = createMemo(() => {
    switch (textBubble()?.color) {
      case "science": return "green";
      case "purple": return "purple";
      default: return "gray";
    }
  });

  // Button → backend cursor move. The "Back" label is the only signal
  // we have today that a button means "go back" (Bubble.alt_button is
  // historically the back-button slot); everything else advances.
  const textBubbleButtonCallback = (button: TextBubbleButton) => {
    appCommand({ type: 'AdvanceBubble', back: button.text === 'Back' });
  };

  const handleMouseDown = (e: MouseEvent) => {
    if (!bubbleRef) return;
    setIsDragging(true);
    const pos = textBubblePosition() || { x: window.innerWidth * 0.2, y: window.innerHeight * 0.2 };
    setDragOffset({ x: e.clientX - pos.x, y: e.clientY - pos.y });
    if (bubbleRef) bubbleRef.style.cursor = 'grabbing';
  };

  const handleMouseMove = (e: MouseEvent) => {
    if (!isDragging()) return;
    const newPos = { x: e.clientX - dragOffset().x, y: e.clientY - dragOffset().y };
    setTextBubblePosition()(newPos);
  };

  const handleMouseUp = () => {
    setIsDragging(false);
    if (bubbleRef) bubbleRef.style.cursor = 'grab';
  };

  onMount(() => {
    window.addEventListener('mousemove', handleMouseMove);
    window.addEventListener('mouseup', handleMouseUp);
    return () => {
      window.removeEventListener('mousemove', handleMouseMove);
      window.removeEventListener('mouseup', handleMouseUp);
    };
  });

  const position = createMemo(() => 
    textBubblePosition() || { x: window.innerWidth * 0.2, y: window.innerHeight * 0.2 }
  );

  return (
    <Show when={textBubble() && isTranslationReady() && hintsVisible()}>
      <div
        ref={bubbleRef}
        style={{
          cursor: 'grab',
          position: 'absolute',
          top: `${position().y}px`,
          left: `${position().x}px`,
          "z-index": 10
        }}
        onMouseDown={handleMouseDown}
      >
        <CalloutBubble
          color={color()}
          tailCoords={tailCoords()}
          showTail={showTail()}
          className={`text-bubble-${color()} pointer-events-auto`}
          style={{ "backdrop-filter": 'blur(12px)' }}
        >
          <div class="text-bubble-contents">
            <pre class="text-bubble-text">{translatedText() || textBubble()?.text}</pre>

            <Show when={textBubble()?.image}>
              <ImageFromFS path={textBubble()!.image!} />
            </Show>

            <div class="text-bubble-buttons">
              <For each={translatedButtons()}>
                {(button) => (
                  <div class="mr-2">
                    <Button
                      shape="block" effect="fog" hoverEffect="scale" activeEffect="translate"
                      color="dark-gray" callback={() => textBubbleButtonCallback(button)}
                    >
                      <div class="m-2 min-w-[100px]">{button.text.toUpperCase()}</div>
                    </Button>
                  </div>
                )}
              </For>
            </div>
          </div>
        </CalloutBubble>
      </div>
    </Show>
  );
};

export default TextBubble;
