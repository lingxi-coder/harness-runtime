import {createLocalAppAgent} from './client.js';
import {fileToContentPart} from './file.js';
import {validateRunAgentInput} from './validators.js';

export {validateRunAgentInput, validateEvent, validateMessage, validateContentPart} from './validators.js';
export {validateLocalAppRunInput} from './client.js';

/** Install the host-scoped resources around the shared official SDK adapter. */
export function installBrowserAgent({transport, tasks, chats, isPersistentThread = () => false}) {
  if (typeof isPersistentThread !== 'function') throw new TypeError('isPersistentThread must be a function.');

  function createClient(options = {}) {
    const threadId = options.threadId;
    const persistentChat = typeof threadId === 'string' && isPersistentThread(threadId) === true;
    const {agentId, description, initialMessages, initialState, debug} = options;
    return createLocalAppAgent({
      transport,
      threadId,
      agentId,
      description,
      initialMessages,
      initialState,
      debug,
      persistentChat,
    });
  }

  async function run(value) {
    const input = validateRunAgentInput(value);
    const persistentChat = isPersistentThread(input.threadId) === true;
    const history = persistentChat ? input.messages.slice(0, -1) : [];
    const client = createLocalAppAgent({
      transport,
      threadId: input.threadId,
      initialMessages: history,
      persistentChat,
    });
    return client.runInput(input);
  }

  return Object.freeze({
    run,
    createClient,
    file: fileToContentPart,
    tasks,
    chats,
  });
}
