export function register(on) {
  on('session.start', async ($, event, next) => {
    const schemaFor = (state) => ({
      type: 'object',
      properties: {
        state: {
          type: 'string',
          const: state,
          description: `source-carried state for ${state}`,
        },
        value: { type: 'integer', minimum: 7 },
      },
      required: ['state', 'value'],
      additionalProperties: false,
    });

    const registrations = [];
    registrations.push(await $.tool.register({
      name: 'deferred_omitted',
      description: '2.1.295 omitted isDeferred candidate',
      inputSchema: schemaFor('deferred_omitted'),
    }));
    registrations.push(await $.tool.register({
      name: 'always_load',
      description: '2.1.295 false isDeferred candidate',
      inputSchema: schemaFor('always_load'),
      isDeferred: false,
    }));
    registrations.push(await $.tool.register({
      name: 'deferred_true',
      description: '2.1.295 true isDeferred candidate',
      inputSchema: schemaFor('deferred_true'),
      isDeferred: true,
    }));

    await $.ui.log(JSON.stringify(registrations), { to: 'transcript' });
    return next(event);
  });
}
