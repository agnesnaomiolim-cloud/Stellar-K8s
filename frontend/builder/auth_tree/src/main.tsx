/**
 * Application entry point for the Soroban Auth Hierarchy Builder.
 */

import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';

import { AuthTreeProvider, AuthTreeBuilder } from './index';

import './styles.css';

const rootElement = document.getElementById('root');

if (!rootElement) {
  throw new Error(
    '[Auth Hierarchy Builder] Root element with id "root" not found in the document. ' +
      'Ensure your index.html contains <div id="root"></div>.',
  );
}

createRoot(rootElement).render(
  <StrictMode>
    <AuthTreeProvider>
      <AuthTreeBuilder />
    </AuthTreeProvider>
  </StrictMode>,
);
