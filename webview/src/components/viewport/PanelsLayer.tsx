import { For } from 'solid-js';

import Panel from '../util/Panel';
import SegmentInfoPanel from '../panels/SegmentInfoPanel';
import { panels } from './panelRegistry';

export default function PanelsLayer() {
	return (
		<div class="z-10">
			<For each={panels()}>
				{(d) => (
					<Panel
						id={d.id}
						title={d.title()}
						width={d.width()}
						position={d.position()}
						triggerId={d.triggerId}
					>
						<d.Body />
					</Panel>
				)}
			</For>
			<SegmentInfoPanel />
		</div>
	);
}
