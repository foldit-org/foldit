import { createSignal, onMount, onCleanup } from 'solid-js';

interface RisingTextProps {
	message: string;
	duration?: number;
	risePixels?: number;
	onComplete?: () => void;
	class?: string;
	style?: Record<string, any>;
}

export default function RisingText(props: RisingTextProps) {
	const {
		message,
		duration = 3000,
		risePixels = 20,
		onComplete,
		class: className = '',
		style = {}
	} = props;
	
	const [isAnimating, setIsAnimating] = createSignal(false);

	onMount(() => {
		const raf = requestAnimationFrame(() => {
			setIsAnimating(true);
		});

		const timer = setTimeout(() => {
			onComplete?.();
		}, duration);

		onCleanup(() => {
			cancelAnimationFrame(raf);
			clearTimeout(timer);
		});
	});

	const { transform: externalTransform, ...restStyle } = style;
	const risingTransform = () => isAnimating() ? `translateY(-${risePixels}px)` : 'translateY(0)';
	const combinedTransform = () => externalTransform
		? `${externalTransform} ${risingTransform()}`
		: risingTransform();

	return (
		<div
			class={`pointer-events-none select-none text-lg font-bold text-white drop-shadow-md ${className}`}
			style={{
				transition: `transform ${duration}ms ease-out, opacity ${duration}ms ease-in`,
				transform: combinedTransform(),
				opacity: isAnimating() ? 0 : 1,
				...restStyle,
			}}
		>
			{message}
		</div>
	);
}
