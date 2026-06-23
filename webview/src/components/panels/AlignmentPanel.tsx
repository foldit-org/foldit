/**
 * SolidJS alignment panel component.
 */

import { Component } from 'solid-js';
import { useBackendData } from '../../services/adapters';
import '../../styles/panels/AlignmentPanel.css';

const AlignmentPanel: Component = () => {
  const alignmentData = useBackendData(state => state.alignmentData);

  return (
    <div class="alignment-content">
      <p>Alignment data: {alignmentData()?.sequence?.length || 0} sequences</p>
    </div>
  );
};

export default AlignmentPanel;
