using System;
using System.IO;
using System.Linq;
using System.Text;
using System.Threading.Tasks;
using Discord;

namespace DrumBot {

    public class ChannelLog {

        /// <summary>
        /// A replacement for all new lines to keep all messages on one line while logging.
        /// </summary>
        const string NewLineReplacement = "\\n";

        const string DateFormat = "yyyy-MM-dd";
        const string FileType = ".log";

        /// <summary>
        /// The absolute path to the directory where all of the logs are stored.
        /// </summary>
        public static readonly string LogDirectory;

        /// <summary>
        /// The directory where all of the logs for specifically the channel described here is stored.
        /// </summary>
        readonly string _serverDirectory;

        /// <summary>
        /// The directory where all of the logs for specifically the channel described here is stored.
        /// </summary>
        readonly string _channelDirectory;

        static ChannelLog() {
            LogDirectory = Path.Combine(Bot.ExecutionDirectory, Config.LogDirectory);
            Log.Info($"Chat Log Directory: { LogDirectory }");
        }

        public static string GuildDirectory(IGuild guild) {
            return Path.Combine(LogDirectory, Check.NotNull(guild).Id.ToString());
        }

        public static string ChannelDirectory(IGuildChannel channel) {
            return Path.Combine(GuildDirectory(channel.Guild), channel.Id.ToString());
        }

        /// <summary>
        /// Gets the path of the log file for this channel on a certain day.
        /// </summary>
        /// <param name="time">the day specified</param>
        /// <returns>the path to the log file</returns>
        public string GetPath(DateTimeOffset time) {
            return GetPath(time.ToString(DateFormat));
        }

        // Same as above, except with direct access.
        public string GetPath(string time) {
            return Path.Combine(_channelDirectory, time) + FileType;
        }

        public ChannelLog(ITextChannel channel) {
            _serverDirectory = GuildDirectory(channel.Guild);
            _channelDirectory = ChannelDirectory(channel);
            if (!Directory.Exists(_channelDirectory)) {
                Directory.CreateDirectory(_channelDirectory);
                Log.Info($"Logs for { channel.Name } do not exist. Downloading the most recent messages.");
                LogChannelRecent(channel);
            }
        }

        public async Task DeletedChannel(ITextChannel channel) {
            if (!Directory.Exists(_channelDirectory))
                return;
            var serverConfig = Config.GetGuildConfig(channel.Guild);
            if(serverConfig.GetChannelConfig(channel).IsIgnored) {
                Log.Info("Ignored channel deleted. Deleting logs...");
                await Utility.FileIO(() => Directory.Delete(_channelDirectory, true));
            } else {
                var targetDirectory = Path.Combine(_serverDirectory,
                    $"Deleted Channel {channel.Name} ({channel.Id})");
                Log.Info("Channel deleted. Moving logs...");
                await Utility.FileIO(() => Directory.Move(_channelDirectory, targetDirectory));
            }
        }

        async void LogChannelRecent(ITextChannel channel) {
            var messages = await channel.GetMessagesAsync();
            foreach (var message in messages.OrderByDescending(m => m.Timestamp))
                await LogMessage(message);
        }

        static string MessageToLog(string message) {
            return message.Replace("\n", NewLineReplacement);
        }

        static string LogToMessage(string log) {
            return log.Replace(NewLineReplacement, "\n");
        }

        /// <summary>
        /// Logs a message.
        /// </summary>
        /// <param name="message">the message to log</param>
        public async Task LogMessage(IMessage message) {
            if(message == null)
                throw new ArgumentNullException();
            var timestamp = message.Timestamp;
            var path = GetPath(timestamp);
            await Utility.FileIO(async delegate {
                using (StreamWriter writer = File.AppendText(path))
                    await writer.WriteLineAsync(MessageToLog($"{Utility.DateString(timestamp)} - { message.ToProcessedString() }"));
            });
        }

        /// <summary>
        /// Searches all logs for instances of a certain exact match.
        /// </summary>
        /// <returns>all matches in a string</returns>
        public Task<string> Search(Func<string, bool> pred) {
            return SearchDirectory(pred, _channelDirectory);
        }

        public static async Task<string> SearchDirectory(Func<string, bool> pred, string directory) {
            if (!Directory.Exists(directory))
                return string.Empty;
            string[] files = Directory.GetFiles(directory);
            string[] results = await Task.WhenAll(files.Select(file => SearchFile(file, pred)));
            return LogToMessage(results.Where(s => !s.IsNullOrEmpty()).Join());
        }

        /// <summary>
        /// Searches a single file for results.
        /// </summary>
        /// <param name="path">the path to the file</param>
        static async Task<string> SearchFile(string path, Func<string, bool> pred) {
            var builder = new StringBuilder();
            Func<Task> read = async delegate {
                using (StreamReader reader = File.OpenText(path)) {
                    while(!reader.EndOfStream) {
                        string line = await reader.ReadLineAsync();
                        if (line != null && pred(line))
                            builder.AppendLine(line);
                    }
                }
            };
            Action retry = delegate { builder.Clear(); };
            await Utility.FileIO(read, retry);
            return builder.ToString();
        }
    }
}
