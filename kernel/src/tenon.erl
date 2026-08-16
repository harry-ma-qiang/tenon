-module(tenon).

-behaviour(gen_server).

-export([start/0, start/1, start_link/0, start_link/1, stop/1, root/1, tree/1,
         status/1, mount/2, unmount/1, restart/1, restart/2, effect/2, on/3, on/4,
         emit/3, call/4, bail/3, provide/3, get/2, svc/4]).

-export([init/1, handle_call/3, handle_cast/2, handle_continue/2, handle_info/2,
         terminate/2, code_change/3]).

-define(DEADLINE, 30000).
-define(GRACE, 5000).
-define(MAX_ARITY, 5).
-define(MAX_FRAME, 1048576).

-type tabs() :: #{fibers := ets:tid(), services := ets:tid(),
                  hooks := ets:tid(), seq := ets:tid()}.
-type ctx() :: #{kernel := pid(), tabs := tabs(), fiber := pid()}.
-type disposer() :: fun(() -> any()).
-type status() :: pending | loading | active | failed | unloading | disposed.
-type spec() :: #{module => module(), cmd => string(), args => [string()],
                  env => [{string(), string()}], config => term(), id => term()}.

-export_type([ctx/0, disposer/0, status/0, spec/0, tabs/0]).

-record(k, {tabs :: tabs(), opts :: map(), root :: pid() | undefined}).

-record(f, {ctx :: ctx(), uid :: integer(), id :: term(), parent :: pid() | undefined,
            module :: module() | undefined, config :: term(), spec :: spec(),
            opts :: map(), inject = [] :: [atom()], epoch = inactive :: term(),
            status = pending :: status(), error :: term(), disposers = [] :: list(),
            phase = ready :: ready | hello | loading, waiters = [] :: list(),
            anchor :: {pid(), reference()} | undefined, port :: port() | undefined,
            os_pid :: integer() | undefined, wire = #{} :: map(), pending = #{} :: map()}).

-spec start() -> {ok, pid()}.
start() ->
    start(#{}).

-spec start(map()) -> {ok, pid()}.
start(Opts) when is_map(Opts) ->
    gen_server:start(?MODULE, {kernel, Opts}, []).

-spec start_link() -> {ok, pid()}.
start_link() ->
    start_link(#{}).

-spec start_link(map()) -> {ok, pid()}.
start_link(Opts) when is_map(Opts) ->
    gen_server:start_link(?MODULE, {kernel, Opts}, []).

-spec stop(pid()) -> ok.
stop(Kernel) ->
    gen_server:stop(Kernel).

-spec root(pid()) -> ctx().
root(Kernel) ->
    gen_server:call(Kernel, root, infinity).

-spec tree(pid()) -> map() | undefined.
tree(Kernel) ->
    gen_server:call(Kernel, tree, infinity).

-spec status(pid()) -> status().
status(Fiber) ->
    try
        gen_server:call(Fiber, status, infinity)
    catch
        exit:{noproc, _} -> disposed
    end.

-spec mount(ctx(), spec()) -> {ok, pid()}.
mount(#{kernel := Kernel, fiber := Owner}, Spec) ->
    Ref = make_ref(),
    {ok, Fiber} = gen_server:call(Kernel, {mount, Spec, Owner, Ref}, infinity),
    ok = attach(Owner, Ref, fun() -> unmount(Fiber) end),
    _ = status(Fiber),
    {ok, Fiber}.

-spec unmount(pid()) -> ok.
unmount(Fiber) ->
    try
        gen_server:call(Fiber, unmount, infinity)
    catch
        exit:_ -> ok
    end.

-spec restart(pid()) -> ok.
restart(Fiber) ->
    gen_server:call(Fiber, restart, infinity).

-spec restart(pid(), term()) -> ok.
restart(Fiber, Config) ->
    gen_server:call(Fiber, {restart, Config}, infinity).

-spec effect(ctx(), fun(() -> disposer() | undefined | ok)) -> disposer().
effect(#{fiber := Owner}, Body) ->
    case Body() of
        Disposer when is_function(Disposer, 0) -> register_effect(Owner, Disposer);
        undefined -> fun() -> ok end;
        ok -> fun() -> ok end;
        Other -> error({bad_effect, Other})
    end.

-spec on(ctx(), term(), function()) -> disposer().
on(Ctx, Event, Fun) ->
    on(Ctx, Event, Fun, #{}).

-spec on(ctx(), term(), function(), map()) -> disposer().
on(#{tabs := Tabs, fiber := Owner} = Ctx, Event, Fun, Opts) when is_function(Fun) ->
    #{hooks := Hooks, seq := Seq} = Tabs,
    effect(Ctx,
           fun() ->
                   Ref = make_ref(),
                   ets:insert(Hooks, {{Event, order(Seq, Opts)}, Ref, Owner, Fun}),
                   fun() -> ets:match_delete(Hooks, {{Event, '_'}, Ref, '_', '_'}) end
           end).

-spec emit(ctx(), term(), [term()]) -> ok.
emit(Ctx, Event, Args) ->
    lists:foreach(fun(Fun) -> isolate(Event, Fun, Args) end, hooks(Ctx, Event)).

-spec call(ctx(), term(), [term()], function()) -> term().
call(Ctx, Event, Args, Terminal) when is_list(Args), is_function(Terminal) ->
    Arity = length(Args),
    Arity =< ?MAX_ARITY orelse error({too_many_args, Arity}),
    Chain = chain(hooks(Ctx, Event), Terminal, Arity),
    Chain(Args).

-spec bail(ctx(), term(), [term()]) -> term().
bail(Ctx, Event, Args) ->
    first_value(hooks(Ctx, Event), Args).

-spec provide(ctx(), atom(), term()) -> disposer().
provide(Ctx, Name, Impl) ->
    effect(Ctx, fun() -> install(Ctx, Name, Impl) end).

-spec get(ctx(), atom()) -> term().
get(#{tabs := #{services := Services}}, Name) ->
    case ets:lookup(Services, Name) of
        [{_, Impl, _}] -> Impl;
        [] -> undefined
    end.

-spec svc(ctx(), atom(), atom(), [term()]) -> term().
svc(Ctx, Name, Method, Args) ->
    case get(Ctx, Name) of
        undefined -> error({no_service, Name});
        {tenon_wire, Fiber, _} ->
            unwrap(gen_server:call(Fiber, {wire_svc, Name, Method, Args}, infinity));
        Impl when is_atom(Impl) -> apply(Impl, Method, Args);
        Impl when is_function(Impl, 2) -> Impl(Method, Args);
        Impl -> error({bad_service, Name, Impl})
    end.

-spec init(term()) -> {ok, #k{} | #f{}} | {ok, #f{}, {continue, mount}}.
init({kernel, Opts}) ->
    process_flag(trap_exit, true),
    Tabs = #{fibers => ets:new(fibers, [set, public, {read_concurrency, true}]),
             services => ets:new(services, [set, public, {read_concurrency, true}]),
             hooks => ets:new(hooks, [ordered_set, public, {read_concurrency, true}]),
             seq => ets:new(seq, [set, public, {write_concurrency, true}])},
    ets:insert(maps:get(seq, Tabs), [{uid, 0}, {append, 0}, {prepend, 0}, {req, 0}]),
    State = #k{tabs = Tabs, opts = options(Opts)},
    {ok, Root} = start_fiber(State, #{}, undefined, undefined),
    {ok, State#k{root = Root}};
init({fiber, Args}) ->
    process_flag(trap_exit, true),
    #{kernel := Kernel, tabs := Tabs, opts := Opts, spec := Spec,
      parent := Parent, ref := Ref} = Args,
    Uid = ets:update_counter(maps:get(seq, Tabs), uid, 1),
    State = #f{ctx = #{kernel => Kernel, tabs => Tabs, fiber => self()},
               uid = Uid, id = maps:get(id, Spec, undefined), parent = Parent,
               module = maps:get(module, Spec, undefined), anchor = anchor(Parent, Ref),
               config = maps:get(config, Spec, undefined), spec = Spec, opts = Opts},
    Loaded = State#f{inject = declared_inject(State)},
    write_row(Loaded),
    case kind(Loaded) of
        root -> {ok, announce(Loaded)};
        _ -> {ok, Loaded, {continue, mount}}
    end.

-spec handle_continue(mount, #f{}) -> {noreply, #f{}}.
handle_continue(mount, #f{} = State) ->
    case kind(State) of
        external -> {noreply, announce(State#f{phase = hello})};
        _ -> {noreply, announce(State)}
    end.

-spec handle_call(term(), gen_server:from(), #k{} | #f{}) -> term().
handle_call(root, _From, #k{tabs = Tabs, root = Root} = State) ->
    {reply, #{kernel => self(), tabs => Tabs, fiber => Root}, State};
handle_call(tree, _From, #k{} = State) ->
    {reply, build_tree(State), State};
handle_call({mount, Spec, Parent, Ref}, _From, #k{} = State) ->
    {reply, start_fiber(State, Spec, Parent, Ref), State};
handle_call({notify, Names}, _From, #k{} = State) ->
    notify(State, Names),
    {reply, ok, State};
handle_call(status, From, #f{} = State0) ->
    State = case settled(State0) of
                true -> refresh(State0);
                false -> State0
            end,
    case settled(State) of
        true -> {reply, State#f.status, State};
        false -> {noreply, State#f{waiters = [From | State#f.waiters]}}
    end;
handle_call({effect, Ref, Disposer}, _From, #f{} = State) ->
    {reply, ok, add_effect(State, Ref, Disposer)};
handle_call({drop, Ref}, _From, #f{} = State) ->
    {reply, ok, run_disposer(State, Ref)};
handle_call(restart, _From, #f{} = State) ->
    {reply, ok, reload(State)};
handle_call({restart, Config}, _From, #f{} = State) ->
    {reply, ok, reload(State#f{config = Config})};
handle_call(unmount, From, #f{} = State) ->
    teardown(State, From);
handle_call({hook_call, Id, Event, Args}, From, #f{} = State) ->
    request(State, From, #{t => <<"hook">>, hook => Id, event => enc(Event),
                           args => enc(Args), mode => <<"call">>});
handle_call({hook_result, Req, Result}, From, #f{} = State) ->
    resume(State, From, Req, Result);
handle_call({wire_svc, Name, Method, Args}, From, #f{} = State) ->
    request(State, From, #{t => <<"svc">>, name => enc(Name), method => enc(Method),
                           args => enc(Args)});
handle_call(_Other, _From, State) ->
    {reply, {error, unknown_request}, State}.

-spec handle_cast(term(), #k{} | #f{}) -> {noreply, #k{} | #f{}} | {stop, term(), #f{}}.
handle_cast(refresh, #f{} = State) ->
    {noreply, settle(refresh(State))};
handle_cast(unmount, #f{} = State) ->
    case teardown(State, undefined) of
        {stop, Reason, _Reply, Final} -> {stop, Reason, Final};
        {reply, _Reply, Idle} -> {noreply, Idle}
    end;
handle_cast({hook_emit, Id, Event, Args}, #f{} = State) ->
    {noreply, notice(State, #{t => <<"hook">>, hook => Id, event => enc(Event),
                              args => enc(Args), mode => <<"emit">>})};
handle_cast({worker_reply, Id, Result}, #f{} = State) ->
    {noreply, notice(State, #{t => <<"rep">>, id => Id, result => enc(Result)})};
handle_cast(_Other, State) ->
    {noreply, State}.

-spec handle_info(term(), #k{} | #f{}) -> {noreply, #k{} | #f{}} | {stop, term(), #f{}}.
handle_info({'EXIT', Pid, _Reason}, #k{tabs = Tabs} = State) when is_pid(Pid) ->
    Names = sweep(Tabs, Pid),
    dispose_children(State, Pid),
    notify(State, Names),
    {noreply, State};
handle_info({tenon_effect, Ref, Disposer}, #f{} = State) ->
    {noreply, add_effect(State, Ref, Disposer)};
handle_info({tenon_drop, Ref}, #f{} = State) ->
    {noreply, run_disposer(State, Ref)};
handle_info({tenon_forget, Ref}, #f{disposers = Ds} = State) ->
    {noreply, State#f{disposers = lists:keydelete(Ref, 1, Ds)}};
handle_info({Port, {data, Bin}}, #f{port = Port} = State) ->
    case byte_size(Bin) > max_frame(State) of
        true ->
            {noreply, reject_frame(Bin, State)};
        false ->
            case decode_frame(Bin) of
                {ok, Frame} -> {noreply, handle_frame(Frame, State)};
                error -> {noreply, State}
            end
    end;
handle_info({Port, {exit_status, Code}}, #f{port = Port} = State) ->
    port_gone(State#f{os_pid = undefined}, {exit_status, Code});
handle_info({'EXIT', Port, Reason}, #f{port = Port} = State) when is_port(Port) ->
    port_gone(State, {port_exit, Reason});
handle_info({'EXIT', Kernel, _Reason}, #f{ctx = #{kernel := Kernel}} = State) ->
    {stop, shutdown, State};
handle_info({tenon_deadline, Req}, #f{} = State) ->
    {noreply, expire(State, Req)};
handle_info(_Other, State) ->
    {noreply, State}.

-spec terminate(term(), #k{} | #f{}) -> ok.
terminate(_Reason, #f{ctx = #{tabs := #{fibers := Fibers}}} = State) ->
    catch ets:delete(Fibers, self()),
    catch close_port(State),
    forget(State#f.anchor),
    ok;
terminate(_Reason, #k{}) ->
    ok.

-spec code_change(term(), #k{} | #f{}, term()) -> {ok, #k{} | #f{}}.
code_change(_Vsn, #k{} = State, _Extra) ->
    {ok, State};
code_change(_Vsn, #f{} = State, _Extra) ->
    {ok, State}.

unwrap({rep, Result}) -> Result;
unwrap(Other) -> Other.

decode_frame(Bin) ->
    try
        {ok, json:decode(Bin)}
    catch
        Class:Reason ->
            logger:error("tenon: bad frame ~p", [{Class, Reason, Bin}]),
            error
    end.

options(Opts) ->
    #{deadline => maps:get(deadline, Opts, ?DEADLINE),
      grace => maps:get(grace, Opts, ?GRACE),
      max_frame => maps:get(max_frame, Opts, env_frame())}.

env_frame() ->
    case string:to_integer(os:getenv("TENON_MAX_FRAME", "")) of
        {Bytes, ""} when Bytes > 0 -> Bytes;
        _ -> ?MAX_FRAME
    end.

start_fiber(#k{tabs = Tabs, opts = Opts}, Spec, Parent, Ref) ->
    gen_server:start_link(?MODULE,
                          {fiber, #{kernel => self(), tabs => Tabs, opts => Opts,
                                    spec => Spec, parent => Parent, ref => Ref}},
                          []).

anchor(undefined, _Ref) -> undefined;
anchor(_Parent, undefined) -> undefined;
anchor(Parent, Ref) -> {Parent, Ref}.

forget(undefined) ->
    ok;
forget({Parent, Ref}) ->
    catch Parent ! {tenon_forget, Ref},
    ok.

sweep(#{hooks := Hooks, services := Services, fibers := Fibers}, Owner) ->
    ets:match_delete(Hooks, {{'_', '_'}, '_', Owner, '_'}),
    Names = [Name || [Name] <- ets:match(Services, {'$1', '_', Owner})],
    lists:foreach(fun(Name) -> ets:delete(Services, Name) end, Names),
    ets:delete(Fibers, Owner),
    Names.

dispose_children(#k{tabs = #{fibers := Fibers}}, Parent) ->
    Children = ets:match(Fibers, {'$1', '_', '_', Parent, '_', '_', '_', '_', '_'}),
    lists:foreach(fun([Child]) -> gen_server:cast(Child, unmount) end, Children).

notify(_State, []) ->
    ok;
notify(#k{tabs = #{fibers := Fibers}}, Names) ->
    Rows = ets:tab2list(Fibers),
    lists:foreach(fun(Row) ->
                          case lists:any(fun(N) -> lists:member(N, Names) end, element(7, Row)) of
                              true -> gen_server:cast(element(1, Row), refresh);
                              false -> ok
                          end
                  end,
                  Rows).

build_tree(#k{tabs = #{fibers := Fibers}, root = Root}) ->
    Rows = ets:tab2list(Fibers),
    case lists:keyfind(Root, 1, Rows) of
        false -> undefined;
        Row -> node_map(Row, Rows)
    end.

node_map({Pid, Uid, Id, Parent, Module, Status, Inject, Epoch, Error}, Rows) ->
    Kids = [R || R <- Rows, element(4, R) =:= Pid],
    #{pid => Pid, uid => Uid, id => Id, parent => Parent, module => Module,
      status => Status, inject => Inject, epoch => Epoch, error => Error,
      children => [node_map(K, Rows) || K <- lists:keysort(2, Kids)]}.

order(Seq, #{prepend := true}) ->
    ets:update_counter(Seq, prepend, -1);
order(Seq, _Opts) ->
    ets:update_counter(Seq, append, 1).

hooks(#{tabs := #{hooks := Hooks}}, Event) ->
    ets:select(Hooks, [{{{Event, '_'}, '_', '_', '$1'}, [], ['$1']}]).

isolate(Event, Fun, Args) ->
    try
        apply(Fun, Args),
        ok
    catch
        Class:Reason:Stack ->
            logger:error("tenon: hook for ~p raised ~p:~p ~p", [Event, Class, Reason, Stack]),
            ok
    end.

first_value([], _Args) ->
    undefined;
first_value([Fun | Rest], Args) ->
    case apply(Fun, Args) of
        undefined -> first_value(Rest, Args);
        Value -> Value
    end.

chain([], Terminal, _Arity) ->
    fun(Args) -> apply(Terminal, Args) end;
chain([Fun | Rest], Terminal, Arity) ->
    fun(Args) -> apply(Fun, Args ++ [next(chain(Rest, Terminal, Arity), Arity)]) end.

next(Cont, 0) -> fun() -> Cont([]) end;
next(Cont, 1) -> fun(A) -> Cont([A]) end;
next(Cont, 2) -> fun(A, B) -> Cont([A, B]) end;
next(Cont, 3) -> fun(A, B, C) -> Cont([A, B, C]) end;
next(Cont, 4) -> fun(A, B, C, D) -> Cont([A, B, C, D]) end;
next(Cont, 5) -> fun(A, B, C, D, E) -> Cont([A, B, C, D, E]) end.

install(#{tabs := #{services := Services}, fiber := Owner} = Ctx, Name, Impl) ->
    case ets:insert_new(Services, {Name, Impl, Owner}) of
        true ->
            publish(Ctx, Name, Impl),
            fun() ->
                    ets:delete(Services, Name),
                    publish(Ctx, Name, undefined)
            end;
        false ->
            error({service_exists, Name})
    end.

publish(#{kernel := Kernel} = Ctx, Name, Impl) ->
    ok = gen_server:call(Kernel, {notify, [Name]}, infinity),
    emit(Ctx, 'internal/service', [Name, Impl]).

attach(Owner, Ref, Disposer) ->
    case self() =:= Owner of
        true ->
            Owner ! {tenon_effect, Ref, Disposer},
            ok;
        false ->
            gen_server:call(Owner, {effect, Ref, Disposer}, infinity)
    end.

register_effect(Owner, Disposer) ->
    Ref = make_ref(),
    ok = attach(Owner, Ref, Disposer),
    fun() -> drop_effect(Owner, Ref) end.

drop_effect(Owner, Ref) ->
    case self() =:= Owner of
        true ->
            Owner ! {tenon_drop, Ref},
            ok;
        false ->
            try
                gen_server:call(Owner, {drop, Ref}, infinity)
            catch
                exit:_ -> ok
            end
    end.

add_effect(#f{disposers = Ds} = State, Ref, Disposer) ->
    State#f{disposers = [{Ref, Disposer} | Ds]}.

run_disposer(#f{disposers = Ds} = State, Ref) ->
    case lists:keytake(Ref, 1, Ds) of
        {value, {_, Disposer}, Rest} ->
            run(Disposer),
            State#f{disposers = Rest};
        false ->
            State
    end.

run(Disposer) ->
    try
        Disposer(),
        ok
    catch
        Class:Reason:Stack ->
            logger:error("tenon: disposer raised ~p:~p ~p", [Class, Reason, Stack]),
            ok
    end.

drain(State) ->
    receive
        {tenon_effect, Ref, Disposer} -> drain(add_effect(State, Ref, Disposer));
        {tenon_drop, Ref} -> drain(run_disposer(State, Ref))
    after 0 ->
        State
    end.

kind(#f{module = Module}) when Module =/= undefined -> inline;
kind(#f{spec = #{cmd := _}}) -> external;
kind(#f{}) -> root.

declared_inject(#f{module = undefined}) ->
    [];
declared_inject(#f{module = Module}) ->
    _ = code:ensure_loaded(Module),
    case erlang:function_exported(Module, inject, 0) of
        true -> Module:inject();
        false -> []
    end.

announce(State) ->
    emit(State#f.ctx, 'internal/plugin', [self()]),
    case kind(State) of
        external -> open_plugin(State);
        _ -> refresh(State)
    end.

settled(#f{phase = Phase}) -> Phase =:= ready.

settle(State) ->
    case settled(State) of
        true -> notify_waiters(State);
        false -> State
    end.

notify_waiters(#f{waiters = []} = State) ->
    State;
notify_waiters(#f{waiters = Waiters, status = Status} = State) ->
    lists:foreach(fun(From) -> gen_server:reply(From, Status) end, Waiters),
    State#f{waiters = []}.

refresh(#f{status = disposed} = State) ->
    State;
refresh(#f{phase = Phase} = State) when Phase =/= ready ->
    State;
refresh(State) ->
    case {compute_epoch(State), State#f.epoch} of
        {Same, Same} -> State;
        {Epoch, inactive} -> do_load(State#f{epoch = Epoch});
        {inactive, _} -> set_status((do_unload(State))#f{epoch = inactive}, pending);
        {Epoch, _} -> do_load((do_unload(State))#f{epoch = Epoch})
    end.

reload(#f{status = disposed} = State) ->
    State;
reload(State) ->
    Idle = (do_unload(State))#f{epoch = inactive, phase = ready},
    settle(refresh(set_status(Idle, pending))).

compute_epoch(#f{inject = Inject, ctx = #{tabs := #{services := Services}}}) ->
    collect(Inject, Services, []).

collect([], _Services, Acc) ->
    lists:reverse(Acc);
collect([Name | Rest], Services, Acc) ->
    case ets:lookup(Services, Name) of
        [{_, _, Owner}] -> collect(Rest, Services, [{Name, Owner} | Acc]);
        [] -> inactive
    end.

do_load(State0) ->
    State = set_status(State0, loading),
    case kind(State) of
        root -> set_status(State, active);
        inline -> finish_load(State, run_load(State));
        external -> spawn_or_load(State)
    end.

spawn_or_load(#f{port = Port} = State) when is_port(Port) ->
    wire_load(State);
spawn_or_load(State) ->
    open_plugin(State#f{phase = hello, wire = #{}}).

run_load(#f{module = Module, ctx = Ctx, config = Config}) ->
    try
        Module:load(Ctx, Config)
    catch
        Class:Reason:Stack -> {error, {Class, Reason, Stack}}
    end.

finish_load(State0, Result) ->
    State = drain(State0),
    case Result of
        ok ->
            set_status(State, active);
        {ok, Disposer} when is_function(Disposer, 0) ->
            set_status(add_effect(State, make_ref(), Disposer), active);
        {error, Reason} ->
            fail(State, Reason);
        Other ->
            fail(State, {bad_return, Other})
    end.

fail(State, Reason) ->
    logger:error("tenon: fiber ~p failed to load: ~p", [self(), Reason]),
    set_status(State#f{error = Reason}, failed).

fail_external(State, Reason) ->
    fail(State#f{epoch = compute_epoch(State)}, Reason).

do_unload(State0) ->
    State1 = set_status(State0, unloading),
    Ds = State1#f.disposers,
    State2 = State1#f{disposers = [], error = undefined},
    lists:foreach(fun({_Ref, Disposer}) -> run(Disposer) end, Ds),
    sweep_own(State2),
    drain((close_plugin(State2))#f{wire = #{}}).

sweep_own(#f{ctx = #{kernel := Kernel, tabs := Tabs}}) ->
    #{hooks := Hooks, services := Services} = Tabs,
    ets:match_delete(Hooks, {{'_', '_'}, '_', self(), '_'}),
    Names = [Name || [Name] <- ets:match(Services, {'$1', '_', self()})],
    lists:foreach(fun(Name) -> ets:delete(Services, Name) end, Names),
    case Names of
        [] -> ok;
        _ -> ok = gen_server:call(Kernel, {notify, Names}, infinity)
    end.

set_status(#f{status = Status} = State, Status) ->
    State;
set_status(State, Status) ->
    Old = State#f.status,
    Updated = State#f{status = Status},
    write_row(Updated),
    emit(Updated#f.ctx, 'internal/status', [self(), Old, Status]),
    Updated.

write_row(#f{ctx = #{tabs := #{fibers := Fibers}}} = State) ->
    Row = {self(), State#f.uid, State#f.id, State#f.parent, State#f.module,
           State#f.status, State#f.inject, State#f.epoch, State#f.error},
    try
        ets:insert(Fibers, Row),
        ok
    catch
        error:badarg -> ok
    end.

teardown(#f{status = disposed} = State, _From) ->
    {reply, ok, State};
teardown(State0, _From) ->
    State = notify_waiters(set_status(do_unload(State0), disposed)),
    delete_row(State),
    {stop, normal, ok, State}.

delete_row(#f{ctx = #{tabs := #{fibers := Fibers}}}) ->
    try
        ets:delete(Fibers, self()),
        ok
    catch
        error:badarg -> ok
    end.

grace(#f{opts = Opts}) -> maps:get(grace, Opts, ?GRACE).

deadline(#f{opts = Opts}) -> maps:get(deadline, Opts, ?DEADLINE).

max_frame(#f{opts = Opts}) -> maps:get(max_frame, Opts, ?MAX_FRAME).

wire_env(State) ->
    [{"TENON_MAX_FRAME", integer_to_list(max_frame(State))},
     {"TENON_KERNEL_DEADLINE", integer_to_list(deadline(State))}].

open_plugin(#f{spec = Spec} = State) ->
    Cmd = maps:get(cmd, Spec),
    Args = maps:get(args, Spec, []),
    Env = maps:get(env, Spec, []) ++ wire_env(State),
    try
        Port = open_port({spawn_executable, Cmd},
                         [{args, Args}, {env, Env}, {packet, 4}, binary, exit_status,
                          nouse_stdio, hide]),
        OsPid = case erlang:port_info(Port, os_pid) of
                    {os_pid, Pid} -> Pid;
                    _ -> undefined
                end,
        arm(State#f{port = Port, os_pid = OsPid}, hello)
    catch
        Class:Reason ->
            settle(fail_external(State#f{phase = ready}, {spawn_failed, Class, Reason}))
    end.

arm(#f{pending = Pending} = State, Req) ->
    Timer = erlang:send_after(deadline(State), self(), {tenon_deadline, Req}),
    State#f{pending = Pending#{Req => {load, Timer}}}.

next_req(#f{ctx = #{tabs := #{seq := Seq}}}) ->
    ets:update_counter(Seq, req, 1).

send_frame(#f{port = Port} = State, Frame) when is_port(Port) ->
    try
        Data = json:encode(Frame),
        Size = iolist_size(Data),
        case Size > max_frame(State) of
            true -> too_large(Frame, Size, State);
            false -> port_command(Port, Data), ok
        end
    catch
        _:_ -> ok
    end;
send_frame(_State, _Frame) ->
    ok.

too_large(Frame, Size, State) ->
    logger:error("tenon: outbound ~p frame of ~p bytes over cap ~p, dropped",
                 [maps:get(t, Frame, undefined), Size, max_frame(State)]),
    {error, frame_too_large}.

notice(State, Frame) ->
    _ = send_frame(State, Frame),
    State.

request(#f{port = Port} = State, From, Frame) when is_port(Port) ->
    Req = next_req(State),
    park(send_frame(State, Frame#{req => Req}), Req, From, State);
request(State, _From, _Frame) ->
    {reply, {error, no_plugin}, State}.

resume(#f{port = Port} = State, From, Req, Result) when is_port(Port) ->
    Frame = #{t => <<"result">>, req => Req, result => enc(Result)},
    park(send_frame(State, Frame), Req, From, State);
resume(State, _From, _Req, _Result) ->
    {reply, {error, no_plugin}, State}.

park(ok, Req, From, State) ->
    Timer = erlang:send_after(deadline(State), self(), {tenon_deadline, Req}),
    {noreply, State#f{pending = maps:put(Req, {call, From, Timer}, State#f.pending)}};
park({error, Reason}, _Req, _From, State) ->
    {reply, {error, Reason}, State}.

wire_load(State0) ->
    Req = next_req(State0),
    State = State0#f{phase = loading},
    Frame = #{t => <<"load">>, req => Req, config => enc(State#f.config)},
    case send_frame(State, Frame) of
        ok -> arm(State, Req);
        {error, Reason} -> settle(fail(State#f{phase = ready}, Reason))
    end.

close_plugin(#f{port = Port} = State) when is_port(Port) ->
    send_frame(State, #{t => <<"unload">>}),
    close_port(await_exit(State, Port));
close_plugin(State) ->
    State.

await_exit(State, Port) ->
    receive
        {Port, {exit_status, _Code}} -> State#f{os_pid = undefined}
    after grace(State) ->
        State
    end.

close_port(#f{port = Port} = State0) when is_port(Port) ->
    State = case drain_exit(Port) of
                true -> State0#f{os_pid = undefined};
                false -> State0
            end,
    catch erlang:port_close(Port),
    ok = kill(State#f.os_pid),
    cancel_all(State#f{port = undefined, os_pid = undefined});
close_port(State) ->
    State.

drain_exit(Port) ->
    receive
        {Port, {exit_status, _Code}} -> true
    after 0 ->
        false
    end.

kill(undefined) ->
    ok;
kill(OsPid) ->
    _ = os:cmd("kill -9 " ++ integer_to_list(OsPid) ++ " 2>/dev/null"),
    ok.

port_gone(State0, Reason) ->
    State1 = do_unload(close_port(State0)),
    {noreply, settle(fail_external(State1#f{phase = ready}, Reason))}.

cancel_all(#f{pending = Pending} = State) ->
    maps:foreach(fun(_Req, Entry) -> cancel(Entry, {error, plugin_gone}) end, Pending),
    State#f{pending = #{}}.

cancel({load, Timer}, _Reply) ->
    _ = erlang:cancel_timer(Timer),
    ok;
cancel({call, From, Timer}, Reply) ->
    _ = erlang:cancel_timer(Timer),
    gen_server:reply(From, Reply),
    ok.

expire(State, Req) ->
    expire(State, Req, timeout).

expire(#f{pending = Pending} = State, Req, Reason) ->
    case maps:take(Req, Pending) of
        {{load, _}, Rest} ->
            settle(fail_external((close_port(State#f{pending = Rest}))#f{phase = ready},
                                 Reason));
        {{call, From, _}, Rest} ->
            gen_server:reply(From, {error, Reason}),
            State#f{pending = Rest};
        error ->
            State
    end.

reject_frame(Bin, State) ->
    logger:error("tenon: inbound frame of ~p bytes over cap ~p, dropped",
                 [byte_size(Bin), max_frame(State)]),
    case decode_frame(Bin) of
        {ok, Frame} -> expire(State, maps:get(<<"req">>, Frame, undefined), frame_too_large);
        error -> State
    end.

take_pending(#f{pending = Pending} = State, Req) ->
    case maps:take(Req, Pending) of
        {Entry, Rest} -> {Entry, State#f{pending = Rest}};
        error -> {none, State}
    end.

handle_frame(#{<<"t">> := <<"hello">>} = Frame, State0) ->
    {Entry, State1} = take_pending(State0, hello),
    cancel_timer(Entry),
    Inject = [to_atom(N) || N <- maps:get(<<"inject">>, Frame, [])],
    State2 = State1#f{inject = Inject, phase = ready},
    write_row(State2),
    settle(resume_load(State2));
handle_frame(#{<<"t">> := <<"on">>} = Frame, State) ->
    wire_on(Frame, State);
handle_frame(#{<<"t">> := <<"off">>} = Frame, State) ->
    wire_off(maps:get(<<"hook">>, Frame), State);
handle_frame(#{<<"t">> := <<"provide">>} = Frame, State) ->
    wire_provide(to_atom(maps:get(<<"name">>, Frame)), State);
handle_frame(#{<<"t">> := <<"unprovide">>} = Frame, State) ->
    wire_off({service, to_atom(maps:get(<<"name">>, Frame))}, State);
handle_frame(#{<<"t">> := <<"emit">>} = Frame, State) ->
    Ctx = State#f.ctx,
    Event = to_atom(maps:get(<<"event">>, Frame)),
    Args = maps:get(<<"args">>, Frame, []),
    _ = spawn(fun() -> emit(Ctx, Event, Args) end),
    State;
handle_frame(#{<<"t">> := <<"call">>} = Frame, State) ->
    Ctx = State#f.ctx,
    Event = to_atom(maps:get(<<"event">>, Frame)),
    Args = maps:get(<<"args">>, Frame, []),
    worker(State, maps:get(<<"id">>, Frame),
           fun() -> call(Ctx, Event, Args, next(fun(Out) -> Out end, length(Args))) end);
handle_frame(#{<<"t">> := <<"svc">>} = Frame, State) ->
    Ctx = State#f.ctx,
    Name = to_atom(maps:get(<<"name">>, Frame)),
    Method = to_atom(maps:get(<<"method">>, Frame)),
    Args = maps:get(<<"args">>, Frame, []),
    worker(State, maps:get(<<"id">>, Frame), fun() -> svc(Ctx, Name, Method, Args) end);
handle_frame(#{<<"t">> := <<"next">>} = Frame, State0) ->
    Req = maps:get(<<"req">>, Frame),
    {Entry, State} = take_pending(State0, Req),
    Await = maps:get(<<"await">>, Frame, false),
    Args = maps:get(<<"args">>, Frame, []),
    answer(Entry, {next, Args, Await, Req}),
    State;
handle_frame(#{<<"t">> := <<"rep">>} = Frame, State0) ->
    Req = maps:get(<<"req">>, Frame),
    {Entry, State} = take_pending(State0, Req),
    reply_frame(Entry, Frame, State);
handle_frame(Frame, State) ->
    logger:error("tenon: unknown frame ~p", [Frame]),
    State.

resume_load(#f{status = loading} = State) ->
    wire_load(State);
resume_load(State) ->
    refresh(State).

cancel_timer({load, Timer}) ->
    _ = erlang:cancel_timer(Timer),
    ok;
cancel_timer(_Entry) ->
    ok.

reply_frame({load, Timer}, Frame, State) ->
    _ = erlang:cancel_timer(Timer),
    case maps:get(<<"error">>, Frame, undefined) of
        undefined -> settle(set_status(State#f{phase = ready}, active));
        Error -> settle(fail(State#f{phase = ready}, {plugin_error, Error}))
    end;
reply_frame({call, _From, _Timer} = Entry, Frame, State) ->
    case maps:get(<<"error">>, Frame, undefined) of
        undefined -> answer(Entry, {rep, maps:get(<<"result">>, Frame, undefined)});
        Error -> answer(Entry, {error, Error})
    end,
    State;
reply_frame(none, _Frame, State) ->
    State.

answer({call, From, Timer}, Reply) ->
    _ = erlang:cancel_timer(Timer),
    gen_server:reply(From, Reply),
    ok;
answer(_Entry, _Reply) ->
    ok.

worker(State, Id, Body) ->
    Fiber = self(),
    _ = spawn(fun() ->
                      Result = try
                                   Body()
                               catch
                                   Class:Reason -> #{error => enc({Class, Reason})}
                               end,
                      gen_server:cast(Fiber, {worker_reply, Id, Result})
              end),
    State.

wire_on(Frame, State) ->
    Id = maps:get(<<"hook">>, Frame),
    Event = to_atom(maps:get(<<"event">>, Frame)),
    Arity = maps:get(<<"arity">>, Frame, 1),
    Mode = maps:get(<<"mode">>, Frame, <<"emit">>),
    Prepend = maps:get(<<"prepend">>, Frame, false),
    Fun = wire_hook(Mode, Arity, self(), Id, Event),
    Disposer = on(State#f.ctx, Event, Fun, #{prepend => Prepend}),
    drain(remember(State, Id, Disposer)).

wire_hook(<<"call">>, Arity, Fiber, Id, Event) ->
    call_hook(Arity, fun(Args, Next) -> wire_waterfall(Fiber, Id, Event, Args, Next) end);
wire_hook(_Mode, Arity, Fiber, Id, Event) ->
    emit_hook(Arity, fun(Args) -> gen_server:cast(Fiber, {hook_emit, Id, Event, Args}) end).

emit_hook(0, G) -> fun() -> G([]) end;
emit_hook(1, G) -> fun(A) -> G([A]) end;
emit_hook(2, G) -> fun(A, B) -> G([A, B]) end;
emit_hook(3, G) -> fun(A, B, C) -> G([A, B, C]) end;
emit_hook(4, G) -> fun(A, B, C, D) -> G([A, B, C, D]) end;
emit_hook(5, G) -> fun(A, B, C, D, E) -> G([A, B, C, D, E]) end.

call_hook(0, G) -> fun(N) -> G([], N) end;
call_hook(1, G) -> fun(A, N) -> G([A], N) end;
call_hook(2, G) -> fun(A, B, N) -> G([A, B], N) end;
call_hook(3, G) -> fun(A, B, C, N) -> G([A, B, C], N) end;
call_hook(4, G) -> fun(A, B, C, D, N) -> G([A, B, C, D], N) end;
call_hook(5, G) -> fun(A, B, C, D, E, N) -> G([A, B, C, D, E], N) end.

wire_waterfall(Fiber, Id, Event, Args, Next) ->
    case gen_server:call(Fiber, {hook_call, Id, Event, Args}, infinity) of
        {rep, Result} ->
            Result;
        {error, _} = Error ->
            Error;
        {next, NewArgs, false, _Req} ->
            apply(Next, rewrite(Args, NewArgs));
        {next, NewArgs, true, Req} ->
            Result = apply(Next, rewrite(Args, NewArgs)),
            case gen_server:call(Fiber, {hook_result, Req, Result}, infinity) of
                {rep, Final} -> Final;
                Other -> Other
            end
    end.

rewrite(Args, NewArgs) when is_list(NewArgs), length(NewArgs) =:= length(Args) ->
    NewArgs;
rewrite(Args, NewArgs) ->
    error({arity_mismatch, length(Args), NewArgs}).

wire_provide(Name, State) ->
    Disposer = provide(State#f.ctx, Name, {tenon_wire, self(), Name}),
    drain(remember(State, {service, Name}, Disposer)).

remember(#f{wire = Wire} = State, Key, Disposer) ->
    State#f{wire = Wire#{Key => Disposer}}.

wire_off(Key, #f{wire = Wire} = State) ->
    case maps:take(Key, Wire) of
        {Disposer, Rest} ->
            run(Disposer),
            drain(State#f{wire = Rest});
        error ->
            State
    end.

to_atom(Value) when is_binary(Value) -> binary_to_atom(Value, utf8);
to_atom(Value) when is_atom(Value) -> Value.

enc(Term) when is_binary(Term); is_number(Term) -> Term;
enc(true) -> true;
enc(false) -> false;
enc(undefined) -> null;
enc(Term) when is_atom(Term) -> atom_to_binary(Term, utf8);
enc(Term) when is_list(Term) -> [enc(E) || E <- Term];
enc(Term) when is_map(Term) ->
    maps:from_list([{enc_key(K), enc(V)} || {K, V} <- maps:to_list(Term)]);
enc(Term) when is_tuple(Term) -> [enc(E) || E <- tuple_to_list(Term)];
enc(Term) -> iolist_to_binary(io_lib:format("~p", [Term])).

enc_key(Key) when is_binary(Key) -> Key;
enc_key(Key) when is_atom(Key) -> atom_to_binary(Key, utf8);
enc_key(Key) -> iolist_to_binary(io_lib:format("~p", [Key])).
