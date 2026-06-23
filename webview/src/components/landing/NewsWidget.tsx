import { createSignal, onMount, Show } from "solid-js";

import { request } from "../../transport";
import { markdownToHtml } from "../../utils/markdown";
import "../../styles/widgets/NewsWidget.css";

interface NewsResponse {
	title: string;
	link: string;
	content: string;
}

export default function NewsWidget() {
	const [news, setNews] = createSignal<NewsResponse | null>(null);

	onMount(() => {
		request<NewsResponse>('server_request', { endpoint: 'get_news', request: {} })
			.then(res => setNews(res))
			.catch(err => console.warn('[NewsWidget] news fetch failed:', err));
	});

	return (
		<Show when={news()}>
			{(currentNews) => {
				const htmlContent = () => markdownToHtml(currentNews().content);
				return (
					<div class="bg-[#333350] bg-opacity-80 text-white p-5 mr-5 mt-4 rounded-lg max-w-md w-full self-start shadow-lg border border-gray-700/50">
						<h1 class="font-bold text-[18px] mb-3 text-white border-b border-gray-600 pb-2">{currentNews().title}</h1>
						<div 
							class="news-markdown text-[14px] mb-4 max-h-[40vh] overflow-y-auto pr-2"
							innerHTML={htmlContent()}
						/>
						{currentNews().link && (
							<div class="pt-2 border-t border-gray-600/50">
								<a 
									class="inline-flex items-center gap-1 text-[13px] font-medium text-blue-300 hover:text-blue-200 hover:underline transition-colors"
									href={currentNews().link}
									target="_blank"
									rel="noopener noreferrer"
								>
									<span>Read full article</span>
									<svg class="w-3 h-3" fill="currentColor" viewBox="0 0 20 20" xmlns="http://www.w3.org/2000/svg">
										<path fill-rule="evenodd" d="M10.293 5.293a1 1 0 011.414 0l4 4a1 1 0 010 1.414l-4 4a1 1 0 01-1.414-1.414L12.586 11H5a1 1 0 110-2h7.586l-2.293-2.293a1 1 0 010-1.414z" clip-rule="evenodd"></path>
									</svg>
								</a>
							</div>
						)}
					</div>
				);
			}}
		</Show>
	);
}
