import { For } from 'solid-js';
import { useUI } from '../../services/adapters';
import { RisingTextMessage } from '../../models/UI';
import RisingText from './RisingText';

export default function RisingTextManager() {
	const risingTextMessages = useUI(state => state.risingTextMessages);
	const removeRisingText = useUI(state => state.removeRisingText);

	return (
		<For each={risingTextMessages()}>
			{(msg: RisingTextMessage) => {
				const getPositionStyle = () => {
					const stackOffset = msg.stackOffset || 0;

					switch (msg.displayMode) {
						case 'huge':
							return {
								position: 'fixed',
								left: '50%',
								top: `calc(50% + ${stackOffset}px)`,
								transform: 'translate(-50%, -50%)',
								'z-index': 9999,
							};
						case 'small':
							return {
								position: 'fixed',
								left: '12.5%',
								top: `calc(75% + ${stackOffset}px)`,
								'z-index': 9999,
							};
						case 'mouse':
						default:
							return {
								position: 'fixed',
								left: `${msg.x}px`,
								top: `${msg.y + stackOffset}px`,
								transform: 'translate(-50%, 0)',
								'z-index': 9999,
							};
					}
				};

				const getTextSizeClass = () => {
					switch (msg.displayMode) {
						case 'huge':
							return 'text-4xl font-bold';
						case 'small':
							return 'text-base';
						case 'mouse':
						default:
							return 'text-lg font-bold';
					}
				};

				const posStyle = getPositionStyle();

				return (
					<RisingText
						message={msg.message}
						duration={msg.duration}
						risePixels={msg.risePixels}
						onComplete={() => removeRisingText()(msg.id)}
						class={getTextSizeClass()}
						style={{
							color: msg.color,
							...posStyle,
						}}
					/>
				);
			}}
		</For>
	);
}
