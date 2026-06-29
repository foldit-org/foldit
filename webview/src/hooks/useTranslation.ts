import { createSignal, createMemo } from 'solid-js';
import { useUI } from '../services/adapters';
import { translationService } from '../services/TranslationService';

interface UseTranslationOptions {
  enabled?: boolean;
  sourceLanguage?: string;
}

export const useTranslation = (options: UseTranslationOptions = {}) => {
  const { enabled = true, sourceLanguage = 'en' } = options;
  const preferredLanguage = useUI(state => state.preferredLanguage);
  const setPreferredLanguage = useUI(state => state.setPreferredLanguage);
  const [isTranslating, setIsTranslating] = createSignal(false);

  /**
   * Translate a single text string
   */
  const translateText = async (text: string): Promise<string> => {
    if (!enabled || !text || preferredLanguage() === sourceLanguage) {
      return text;
    }

    setIsTranslating(true);
    try {
      const translatedText = await translationService.translateText(text, preferredLanguage(), sourceLanguage);
      return translatedText;
    } catch (error) {
      console.warn('Translation failed:', error);
      return text;
    } finally {
      setIsTranslating(false);
    }
  };

  /**
   * Check if translation is needed
   */
  const needsTranslation = createMemo(() => preferredLanguage() !== sourceLanguage && enabled);

  return {
    currentLanguage: preferredLanguage,
    setLanguage: setPreferredLanguage,
    translateText,
    isTranslating,
    needsTranslation,
  };
};