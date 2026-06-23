// src/hooks/useMatomo.ts
import { onMount } from "solid-js";

import { initMatomo, trackPageView } from "../services/Matomo";

export const useMatomo = () => {
	onMount(() => {
		initMatomo();
		trackPageView(); // track initial load of play.fold.it
	});
};

