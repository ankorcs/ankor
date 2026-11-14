import document from '@ankorcs/protocol/asyncapi.yaml?raw';

// The package builds the document to be read on its own, so its links already carry the site's address.
export function GET(): Response {
  return new Response(document, { headers: { 'Content-Type': 'application/yaml' } });
}
