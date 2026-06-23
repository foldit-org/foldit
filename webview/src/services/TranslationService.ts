/**
 * Translation Service for Foldit Web GUI
 * Provides Google Translate integration specifically for text bubbles.
 * Uses unofficial Google Translate API with robust chunking & formatting.
 */

export class TranslationService {
	private static instance: TranslationService;

	private translationCache = new Map<string, string>();


	private constructor() { }

	static getInstance(): TranslationService {
		if (!TranslationService.instance) {
			TranslationService.instance = new TranslationService();
		}
		return TranslationService.instance;
	}

	private preserveFormatting(original: string, translated: string): string {
		if (original === original.toUpperCase() && original !== original.toLowerCase()) {
			return translated.toUpperCase();
		}
		if (this.isTitleCase(original)) {
			return this.toTitleCase(translated);
		}
		if (original.length > 0 && original[0] === original[0].toUpperCase()) {
			return translated.charAt(0).toUpperCase() + translated.slice(1);
		}
		return translated;
	}

	private isTitleCase(text: string): boolean {
		const words = text.split(" ");

		return words.every(w => !w || w[0] === w[0].toUpperCase());
	}

	private toTitleCase(text: string): string {
		return text.replace(/\w\S*/g, t => t.charAt(0).toUpperCase() + t.substr(1).toLowerCase());
	}


	private splitTextForTranslation(text: string): { chunks: string[]; delimiters: string[] } {
		if (text.length <= 100) {
			return { chunks: [text], delimiters: [""] };
		}

		// Split on sentence endings, capturing punctuation + whitespace as delimiters
		const sentences = text.split(/(?<=[.!?])(\s+)/);
		const chunks: string[] = [];
		const delimiters: string[] = [];

		let currentChunk = "";
		let currentDelimiter = "";

		for (let i = 0; i < sentences.length; i += 2) {
			const sentence = sentences[i];
			const delimiter = sentences[i + 1] || "";


			if (currentChunk.length + sentence.length > 150 && currentChunk.length > 0) {
				chunks.push(currentChunk.trim());
				delimiters.push(currentDelimiter);
				currentChunk = sentence;

				currentDelimiter = delimiter;
			} else {
				currentChunk += sentence + delimiter;
			}
		}

		if (currentChunk.trim()) {
			chunks.push(currentChunk.trim());
			delimiters.push(currentDelimiter);
		}

		return { chunks, delimiters };
	}

	private async translateChunk(chunk: string, targetLang: string, sourceLang: string): Promise<string> {

		const cacheKey = `${chunk}_${sourceLang}_${targetLang}`;
		if (this.translationCache.has(cacheKey)) {
			return this.translationCache.get(cacheKey)!;
		}

		const url = `https://translate.googleapis.com/translate_a/single?client=gtx&sl=${sourceLang}&tl=${targetLang}&dt=t&q=${encodeURIComponent(chunk)}`;

		for (let attempt = 0; attempt < 3; attempt++) {
			try {
				const response = await fetch(url);
				if (!response.ok) throw new Error(`HTTP ${response.status}`);
				const result = await response.json();

				let translated = "";
				if (result[0] && Array.isArray(result[0])) {
					translated = result[0].map((seg: any) => seg[0] || "").join("");
				} else {
					translated = result[0]?.[0]?.[0] || "";
				}

				if (!translated.trim()) throw new Error("Empty translation result");

				const formatted = this.preserveFormatting(chunk, translated);
				this.translationCache.set(cacheKey, formatted);
				return formatted;
			} catch (err) {
				if (attempt === 2) {
					console.warn("Translation failed, keeping original:", chunk);
					return chunk;
				}

				await new Promise(res => setTimeout(res, 200 * (attempt + 1)));
			}
		}

		return chunk;
	}

	async translateText(text: string, targetLang: string, sourceLang = "en"): Promise<string> {
		if (targetLang === sourceLang || targetLang === "en") return text;

		const cacheKey = `${text}_${sourceLang}_${targetLang}`;
		if (this.translationCache.has(cacheKey)) {
			return this.translationCache.get(cacheKey)!;
		}

		const { chunks, delimiters } = this.splitTextForTranslation(text);
		const translatedChunks: string[] = [];

		for (const chunk of chunks) {
			translatedChunks.push(await this.translateChunk(chunk, targetLang, sourceLang));
		}

		let combined = "";
		for (let i = 0; i < translatedChunks.length; i++) {
			combined += translatedChunks[i] + (delimiters[i] || "");
		}

		combined = combined.replace(/[ \t]+/g, " ").replace(/\s+\n/g, "\n").trim();

		this.translationCache.set(cacheKey, combined);
		return combined;
	}


	clearCache(): void {
		this.translationCache.clear();
	}
}

export const translationService = TranslationService.getInstance();

