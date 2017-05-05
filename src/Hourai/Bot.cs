using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Logging;
using System;
using System.Collections.Generic;
using System.Collections.Concurrent;
using System.IO;
using System.Linq;
using System.Reflection;
using System.Threading.Tasks;
using Discord;
using Discord.Commands;
using Discord.WebSocket;
namespace Hourai {

  public class BotCounters {

    public SimpleCounter Reconnects { get; }

    public BotCounters() {
      Reconnects = new SimpleCounter();
    }

  }

  public class Bot {

    static void Main() => new Bot().RunAsync().GetAwaiter().GetResult();

    public static IUser Owner { get; private set; }

    public DateTime StartTime { get; private set; }
    public TimeSpan Uptime => DateTime.Now - StartTime;

    readonly ILoggerFactory _loggerFactory;
    readonly ILogger _log;

    DiscordShardedClient Client { get; set; }
    ErrorService ErrorService { get; set; }

    static TaskCompletionSource<object> ExitSource { get; set; }
    bool _initialized;

    public static event Func<Task> RegularTasks {
      add { _regularTasks.Add(Check.NotNull(value)); }
      remove { _regularTasks.Remove(value); }
    }

    static List<Func<Task>> _regularTasks;

    public Bot() {
      ExitSource = new TaskCompletionSource<object>();
      _regularTasks = new List<Func<Task>>();
      _loggerFactory = new LoggerFactory()
        .AddConsole()
        .AddDebug();
      _log = _loggerFactory.CreateLogger("Hourai");
      Config.Load();
    }

    public void Exit() {
      _log.LogInformation("Bot exit has registered. Will exit on next cycle.");
      ExitSource.SetResult(new object());
    }

    async Task Initialize() {
      if (_initialized)
        return;
      StartTime = DateTime.Now;
      _log.LogInformation("Initializing...");
      Client = new DiscordShardedClient(Config.DiscordConfig);
      var commands = new CommandService(new CommandServiceConfig() {
        DefaultRunMode = RunMode.Sync
      });
      var services = new ServiceCollection();
      services.AddSingleton(this);
      services.AddSingleton(Client);
      services.AddSingleton(commands);

      services.AddSingleton(_loggerFactory);

      services.AddSingleton(new CounterSet(new ActivatorFactory<SimpleCounter>()));
      services.AddSingleton(new BotCounters());
      services.AddSingleton(new LogSet());
      var entryAssembly = Assembly.GetEntryAssembly();
      await commands.AddModulesAsync(entryAssembly);

      _log.LogInformation("Loading Services...");
      var foundServices = ServiceDiscovery.FindServices(entryAssembly);
      foreach(var serviceType in foundServices) {
        services.AddSingleton(serviceType);
        _log.LogInformation($"Registered {serviceType.Name}");
      }
      var provider = new DefaultServiceProviderFactory().CreateServiceProvider(services);
      foreach(var serviceType in foundServices) {
        provider.GetService(serviceType);
        _log.LogInformation($"Loaded {serviceType.Name}");
      }
      _log.LogInformation("Services loaded.");
      ErrorService = provider.GetService<ErrorService>();
      _initialized = true;
    }

    async Task MainLoop() {
      while (!ExitSource.Task.IsCompleted) {
        _log.LogInformation("Starting regular tasks...");
        var tasks = Task.WhenAll(_regularTasks.Select(t => t()));
        _log.LogInformation("Waiting...");
        await Task.WhenAny(Task.Delay(60000), ExitSource.Task);
      }
    }

    async Task RunAsync() {
      await Initialize();
      _log.LogInformation("Logging into Discord...");
      await Client.LoginAsync(TokenType.Bot, Config.Token, false);
      _log.LogInformation("Starting Discord Client...");
      await Client.StartAsync();
      _log.LogInformation($"Logged in as {Client.CurrentUser.ToIDString()}");

      Owner = (await Client.GetApplicationInfoAsync()).Owner;
      _log.LogInformation($"Owner: {Owner.ToIDString()}");
      //await Client.SetGameAsync(Config.Version);
      try {
        while (!ExitSource.Task.IsCompleted) {
          try {
            await MainLoop();
          } catch (Exception error) {
            _log.LogError(0, error, "Bot error.");
            ErrorService.RegisterException(error);
          }
        }
      } finally {
        _log.LogInformation("Logging out...");
        await Client.LogoutAsync();
        _log.LogInformation("Stopping Discord client...");
        await Client.StopAsync();
      }
    }

  }

}